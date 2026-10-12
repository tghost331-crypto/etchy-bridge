//! Etchy Rune bridge (M8): a LOCK VAULT between an Etchy SPL token on Solana
//! and the real Bitcoin Rune held in Etchy custody.
//!
//! Every Etchy coin has a FIXED supply equal to the Rune premine (no mint
//! authority, no freeze authority), so this program never mints or burns:
//!  - Bridge to Rune: the holder locks tokens in the coin's vault and names a
//!    Taproot (bc1p) address; Etchy's attester sends the Rune on Bitcoin and
//!    records the Bitcoin txid (or refunds the lock if it cannot be sent).
//!  - Bridge to Solana: after a Rune deposit to Etchy custody is final on
//!    Bitcoin, the attester releases the same amount from the vault. Each
//!    (coin, Bitcoin outpoint) can be released exactly once (receipt PDA).
//! Invariant: Rune in custody == token supply - vault balance.
//!
//! Safety: the attester can only move vault tokens to a deposit's recipient
//! (once per outpoint, under a per-coin daily cap) or back to the original
//! locker (refund). The admin sets keys and limits but can never move
//! tokens; the guardian can only PAUSE; only the admin can unpause.
use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("BGEwwP6EcQAvBqZhYQXPk5jPKpiBXFXqWKag9GsZpDx2");

// Who to contact about a security issue (shown by Solana explorers).
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Etchy Bridge",
    project_url: "https://etchy.fun",
    contacts: "email:security@etchy.fun",
    policy: "https://github.com/tghost331-crypto/etchy-bridge/blob/main/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/tghost331-crypto/etchy-bridge"
}

/// Etchy coin shape (= Rune premine at divisibility 6).
pub const ETCHY_DECIMALS: u8 = 6;
pub const ETCHY_SUPPLY: u64 = 1_000_000_000_000_000;
pub const SECONDS_PER_DAY: i64 = 86_400;
/// Upper bound on the SOL fee charged per Bridge-to-Rune (covers BTC fees).
pub const MAX_WITHDRAWAL_FEE_LAMPORTS: u64 = 1_000_000_000;

#[program]
pub mod etchy_bridge {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, args: InitializeArgs) -> Result<()> {
        require!(
            args.withdrawal_fee_lamports <= MAX_WITHDRAWAL_FEE_LAMPORTS,
            BridgeError::FeeTooHigh
        );
        let c = &mut ctx.accounts.config;
        c.admin = args.admin;
        c.pending_admin = Pubkey::default();
        c.guardian = args.guardian;
        c.attester = args.attester;
        c.fee_receiver = args.fee_receiver;
        c.withdrawal_fee_lamports = args.withdrawal_fee_lamports;
        c.paused = false;
        c.bump = ctx.bumps.config;
        Ok(())
    }

    /// Admin: register an Etchy coin. The mint must be an Etchy coin: 6
    /// decimals, no mint and no freeze authority, supply at most the 10^15
    /// Rune premine (it can only shrink, by burns, so the Rune backing in
    /// custody always covers every token).
    pub fn register_mint(ctx: Context<RegisterMint>, limits: MintLimits) -> Result<()> {
        let mint = &ctx.accounts.mint;
        require!(
            mint.decimals == ETCHY_DECIMALS
                && mint.supply > 0
                && mint.supply <= ETCHY_SUPPLY
                && mint.mint_authority.is_none()
                && mint.freeze_authority.is_none(),
            BridgeError::NotAnEtchyCoin
        );
        validate_limits(&limits)?;
        let b = &mut ctx.accounts.mint_bridge;
        b.mint = mint.key();
        b.vault = ctx.accounts.vault.key();
        b.rune_block = 0;
        b.rune_tx = 0;
        b.enabled = false;
        b.paused = false;
        b.min_withdrawal = limits.min_withdrawal;
        b.daily_release_cap = limits.daily_release_cap;
        b.window_day = 0;
        b.released_in_window = 0;
        b.withdrawal_seq = 0;
        b.total_locked = 0;
        b.total_released = 0;
        b.total_refunded = 0;
        b.bump = ctx.bumps.mint_bridge;
        b.vault_bump = ctx.bumps.vault;
        emit!(MintRegistered { mint: b.mint, vault: b.vault });
        Ok(())
    }

    /// Admin: link the coin to its Rune once the etch is FINAL on Bitcoin.
    /// Bridging (both ways) is impossible before this, and the Rune ID can
    /// never be changed afterwards.
    pub fn enable_rune(ctx: Context<AdminMint>, rune_block: u64, rune_tx: u32) -> Result<()> {
        let b = &mut ctx.accounts.mint_bridge;
        require!(!b.enabled, BridgeError::AlreadyEnabled);
        require!(rune_block > 0, BridgeError::InvalidRuneId);
        b.rune_block = rune_block;
        b.rune_tx = rune_tx;
        b.enabled = true;
        emit!(RuneEnabled { mint: b.mint, rune_block, rune_tx });
        Ok(())
    }

    /// Holder: lock tokens and request the real Rune at a Taproot address.
    /// `btc_witness_program` is the 32-byte segwit-v1 program of a bc1p address.
    /// `request_id` is any client-chosen number unique for this user (random);
    /// it keeps concurrent requests from different users independent.
    pub fn bridge_to_rune(
        ctx: Context<BridgeToRune>,
        request_id: u64,
        amount: u64,
        btc_witness_program: [u8; 32],
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        let b = &ctx.accounts.mint_bridge;
        require!(!config.paused && !b.paused, BridgeError::Paused);
        require!(b.enabled, BridgeError::NotEnabled);
        require!(amount >= b.min_withdrawal && amount > 0, BridgeError::AmountTooSmall);
        require!(
            btc_witness_program != [0u8; 32],
            BridgeError::InvalidBtcAddress
        );
        if config.withdrawal_fee_lamports > 0 {
            system_program::transfer(
                CpiContext::new(
                    system_program::ID,
                    system_program::Transfer {
                        from: ctx.accounts.user.to_account_info(),
                        to: ctx.accounts.fee_receiver.to_account_info(),
                    },
                ),
                config.withdrawal_fee_lamports,
            )?;
        }
        token::transfer(
            CpiContext::new(
                token::ID,
                Transfer {
                    from: ctx.accounts.user_token.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;
        let seq = b.withdrawal_seq;
        let w = &mut ctx.accounts.withdrawal;
        w.mint = b.mint;
        w.seq = seq;
        w.request_id = request_id;
        w.user = ctx.accounts.user.key();
        w.user_token = ctx.accounts.user_token.key();
        w.amount = amount;
        w.btc_witness_program = btc_witness_program;
        w.status = WithdrawalStatus::Pending;
        w.btc_txid = [0u8; 32];
        w.requested_at = Clock::get()?.unix_timestamp;
        w.bump = ctx.bumps.withdrawal;
        let b = &mut ctx.accounts.mint_bridge;
        b.withdrawal_seq = seq.checked_add(1).ok_or(BridgeError::Overflow)?;
        b.total_locked = b.total_locked.checked_add(amount).ok_or(BridgeError::Overflow)?;
        emit!(WithdrawalRequested {
            mint: b.mint,
            seq,
            request: w.key(),
            user: w.user,
            amount,
            btc_witness_program,
        });
        Ok(())
    }

    /// Holder: close a finished (Sent or Refunded) request and get its rent back.
    pub fn close_withdrawal(ctx: Context<CloseWithdrawal>) -> Result<()> {
        require!(
            ctx.accounts.withdrawal.status != WithdrawalStatus::Pending,
            BridgeError::StillPending
        );
        Ok(())
    }

    /// Attester: the Rune was sent on Bitcoin (records the txid, final state).
    pub fn mark_withdrawal_sent(ctx: Context<AttestWithdrawal>, btc_txid: [u8; 32]) -> Result<()> {
        require!(btc_txid != [0u8; 32], BridgeError::InvalidBtcTxid);
        let w = &mut ctx.accounts.withdrawal;
        require!(w.status == WithdrawalStatus::Pending, BridgeError::NotPending);
        w.status = WithdrawalStatus::Sent;
        w.btc_txid = btc_txid;
        emit!(WithdrawalSent { mint: w.mint, seq: w.seq, btc_txid });
        Ok(())
    }

    /// Attester: the Rune could not be sent; return the locked tokens to the
    /// requester's own wallet (its canonical token account, re-created if the
    /// user closed it). Final state; exclusive with Sent.
    pub fn refund_withdrawal(ctx: Context<RefundWithdrawal>) -> Result<()> {
        let w = &mut ctx.accounts.withdrawal;
        require!(w.status == WithdrawalStatus::Pending, BridgeError::NotPending);
        w.status = WithdrawalStatus::Refunded;
        let amount = w.amount;
        let (mint, seq) = (w.mint, w.seq);
        let b = &mut ctx.accounts.mint_bridge;
        b.total_refunded = b.total_refunded.checked_add(amount).ok_or(BridgeError::Overflow)?;
        let bump = [b.bump];
        let seeds: &[&[u8]] = &[b"mint".as_ref(), mint.as_ref(), &bump];
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.user_token.to_account_info(),
                    authority: ctx.accounts.mint_bridge.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        emit!(WithdrawalRefunded { mint, seq, amount });
        Ok(())
    }

    /// Attester: release tokens for a FINAL Rune deposit to Etchy custody.
    /// The receipt PDA (coin, txid, vout) makes every outpoint single-use.
    pub fn release_deposit(
        ctx: Context<ReleaseDeposit>,
        btc_txid: [u8; 32],
        vout: u32,
        amount: u64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused && !ctx.accounts.mint_bridge.paused, BridgeError::Paused);
        require!(ctx.accounts.mint_bridge.enabled, BridgeError::NotEnabled);
        require!(amount > 0, BridgeError::AmountTooSmall);
        require!(btc_txid != [0u8; 32], BridgeError::InvalidBtcTxid);
        let now = Clock::get()?.unix_timestamp;
        let day = now.div_euclid(SECONDS_PER_DAY);
        let b = &mut ctx.accounts.mint_bridge;
        if b.window_day != day {
            b.window_day = day;
            b.released_in_window = 0;
        }
        let in_window = b.released_in_window.checked_add(amount).ok_or(BridgeError::Overflow)?;
        require!(in_window <= b.daily_release_cap, BridgeError::DailyCapExceeded);
        b.released_in_window = in_window;
        b.total_released = b.total_released.checked_add(amount).ok_or(BridgeError::Overflow)?;
        let r = &mut ctx.accounts.receipt;
        r.mint = b.mint;
        r.btc_txid = btc_txid;
        r.vout = vout;
        r.amount = amount;
        r.recipient = ctx.accounts.recipient.key();
        r.released_at = now;
        r.bump = ctx.bumps.receipt;
        let mint = b.mint;
        let bump = [b.bump];
        let seeds: &[&[u8]] = &[b"mint".as_ref(), mint.as_ref(), &bump];
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.recipient_token.to_account_info(),
                    authority: ctx.accounts.mint_bridge.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        emit!(DepositReleased {
            mint,
            btc_txid,
            vout,
            amount,
            recipient: r.recipient,
        });
        Ok(())
    }

    /// Admin or guardian: pause everything (fast incident response).
    pub fn pause(ctx: Context<Pause>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        let who = ctx.accounts.authority.key();
        require!(who == c.admin || who == c.guardian, BridgeError::Unauthorized);
        c.paused = true;
        emit!(PausedChanged { paused: true, by: who });
        Ok(())
    }

    /// Admin only: unpause.
    pub fn unpause(ctx: Context<AdminConfig>) -> Result<()> {
        ctx.accounts.config.paused = false;
        emit!(PausedChanged { paused: false, by: ctx.accounts.admin.key() });
        Ok(())
    }

    /// Admin: pause/unpause a single coin.
    pub fn set_mint_paused(ctx: Context<AdminMint>, paused: bool) -> Result<()> {
        ctx.accounts.mint_bridge.paused = paused;
        Ok(())
    }

    /// Admin: rotate keys / fee (attester rotation is the compromise response).
    pub fn update_config(ctx: Context<AdminConfig>, args: UpdateConfigArgs) -> Result<()> {
        require!(
            args.withdrawal_fee_lamports <= MAX_WITHDRAWAL_FEE_LAMPORTS,
            BridgeError::FeeTooHigh
        );
        let c = &mut ctx.accounts.config;
        c.guardian = args.guardian;
        c.attester = args.attester;
        c.fee_receiver = args.fee_receiver;
        c.withdrawal_fee_lamports = args.withdrawal_fee_lamports;
        emit!(ConfigUpdated {
            attester: c.attester,
            guardian: c.guardian,
            fee_receiver: c.fee_receiver,
            withdrawal_fee_lamports: c.withdrawal_fee_lamports,
        });
        Ok(())
    }

    /// Admin: per-coin limits.
    pub fn set_mint_limits(ctx: Context<AdminMint>, limits: MintLimits) -> Result<()> {
        validate_limits(&limits)?;
        let b = &mut ctx.accounts.mint_bridge;
        b.min_withdrawal = limits.min_withdrawal;
        b.daily_release_cap = limits.daily_release_cap;
        Ok(())
    }

    /// Admin transfer, step 1 (the new admin must accept).
    pub fn propose_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        require!(new_admin != Pubkey::default(), BridgeError::Unauthorized);
        ctx.accounts.config.pending_admin = new_admin;
        Ok(())
    }

    /// Admin transfer, step 2.
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(
            c.pending_admin != Pubkey::default() && c.pending_admin == ctx.accounts.new_admin.key(),
            BridgeError::Unauthorized
        );
        c.admin = c.pending_admin;
        c.pending_admin = Pubkey::default();
        Ok(())
    }
}

fn validate_limits(l: &MintLimits) -> Result<()> {
    require!(
        l.min_withdrawal > 0 && l.daily_release_cap > 0 && l.daily_release_cap <= ETCHY_SUPPLY,
        BridgeError::InvalidLimits
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
#[account]
#[derive(InitSpace)]
pub struct BridgeConfig {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    pub guardian: Pubkey,
    pub attester: Pubkey,
    pub fee_receiver: Pubkey,
    pub withdrawal_fee_lamports: u64,
    pub paused: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct MintBridge {
    pub mint: Pubkey,
    pub vault: Pubkey,
    /// Rune ID `block:tx` (set once, when the etch is final).
    pub rune_block: u64,
    pub rune_tx: u32,
    pub enabled: bool,
    pub paused: bool,
    pub min_withdrawal: u64,
    pub daily_release_cap: u64,
    pub window_day: i64,
    pub released_in_window: u64,
    pub withdrawal_seq: u64,
    pub total_locked: u64,
    pub total_released: u64,
    pub total_refunded: u64,
    pub bump: u8,
    pub vault_bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace, Debug)]
pub enum WithdrawalStatus {
    Pending,
    Sent,
    Refunded,
}

#[account]
#[derive(InitSpace)]
pub struct WithdrawalRequest {
    pub mint: Pubkey,
    /// Per-coin order of requests (the attester processes in this order).
    pub seq: u64,
    pub request_id: u64,
    pub user: Pubkey,
    pub user_token: Pubkey,
    pub amount: u64,
    pub btc_witness_program: [u8; 32],
    pub status: WithdrawalStatus,
    pub btc_txid: [u8; 32],
    pub requested_at: i64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct DepositReceipt {
    pub mint: Pubkey,
    pub btc_txid: [u8; 32],
    pub vout: u32,
    pub amount: u64,
    pub recipient: Pubkey,
    pub released_at: i64,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitializeArgs {
    pub admin: Pubkey,
    pub guardian: Pubkey,
    pub attester: Pubkey,
    pub fee_receiver: Pubkey,
    pub withdrawal_fee_lamports: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct UpdateConfigArgs {
    pub guardian: Pubkey,
    pub attester: Pubkey,
    pub fee_receiver: Pubkey,
    pub withdrawal_fee_lamports: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct MintLimits {
    pub min_withdrawal: u64,
    pub daily_release_cap: u64,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + BridgeConfig::INIT_SPACE,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, BridgeConfig>,
    /// Only the program's upgrade authority may initialize (no front-running).
    #[account(
        constraint = program.programdata_address()? == Some(program_data.key())
            @ BridgeError::Unauthorized
    )]
    pub program: Program<'info, crate::program::EtchyBridge>,
    #[account(
        constraint = program_data.upgrade_authority_address == Some(payer.key())
            @ BridgeError::Unauthorized
    )]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterMint<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = admin @ BridgeError::Unauthorized)]
    pub config: Account<'info, BridgeConfig>,
    pub mint: Account<'info, Mint>,
    #[account(
        init,
        payer = admin,
        space = 8 + MintBridge::INIT_SPACE,
        seeds = [b"mint", mint.key().as_ref()],
        bump
    )]
    pub mint_bridge: Account<'info, MintBridge>,
    #[account(
        init,
        payer = admin,
        token::mint = mint,
        token::authority = mint_bridge,
        seeds = [b"vault", mint.key().as_ref()],
        bump
    )]
    pub vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminMint<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = admin @ BridgeError::Unauthorized)]
    pub config: Account<'info, BridgeConfig>,
    #[account(mut, seeds = [b"mint", mint_bridge.mint.as_ref()], bump = mint_bridge.bump)]
    pub mint_bridge: Account<'info, MintBridge>,
}

#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin @ BridgeError::Unauthorized)]
    pub config: Account<'info, BridgeConfig>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub new_admin: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, BridgeConfig>,
}

#[derive(Accounts)]
pub struct Pause<'info> {
    pub authority: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, BridgeConfig>,
}

#[derive(Accounts)]
#[instruction(request_id: u64, amount: u64, btc_witness_program: [u8; 32])]
pub struct BridgeToRune<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = fee_receiver @ BridgeError::InvalidFeeReceiver)]
    pub config: Account<'info, BridgeConfig>,
    /// CHECK: lamport destination pinned to config.fee_receiver via has_one.
    #[account(mut)]
    pub fee_receiver: UncheckedAccount<'info>,
    #[account(
        mut,
        seeds = [b"mint", mint_bridge.mint.as_ref()],
        bump = mint_bridge.bump,
        has_one = vault
    )]
    pub mint_bridge: Box<Account<'info, MintBridge>>,
    #[account(
        mut,
        token::mint = mint_bridge.mint,
        token::authority = user
    )]
    pub user_token: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = user,
        space = 8 + WithdrawalRequest::INIT_SPACE,
        seeds = [b"withdrawal", mint_bridge.mint.as_ref(), user.key().as_ref(), &request_id.to_le_bytes()],
        bump
    )]
    pub withdrawal: Box<Account<'info, WithdrawalRequest>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseWithdrawal<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(
        mut,
        close = user,
        has_one = user @ BridgeError::Unauthorized,
        seeds = [b"withdrawal", withdrawal.mint.as_ref(), withdrawal.user.as_ref(), &withdrawal.request_id.to_le_bytes()],
        bump = withdrawal.bump
    )]
    pub withdrawal: Account<'info, WithdrawalRequest>,
}

#[derive(Accounts)]
pub struct AttestWithdrawal<'info> {
    pub attester: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = attester @ BridgeError::Unauthorized)]
    pub config: Account<'info, BridgeConfig>,
    #[account(
        mut,
        seeds = [b"withdrawal", withdrawal.mint.as_ref(), withdrawal.user.as_ref(), &withdrawal.request_id.to_le_bytes()],
        bump = withdrawal.bump
    )]
    pub withdrawal: Account<'info, WithdrawalRequest>,
}

#[derive(Accounts)]
pub struct RefundWithdrawal<'info> {
    #[account(mut)]
    pub attester: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = attester @ BridgeError::Unauthorized)]
    pub config: Account<'info, BridgeConfig>,
    #[account(
        mut,
        seeds = [b"withdrawal", withdrawal.mint.as_ref(), withdrawal.user.as_ref(), &withdrawal.request_id.to_le_bytes()],
        bump = withdrawal.bump,
        has_one = user @ BridgeError::WrongRefundAccount,
        has_one = mint
    )]
    pub withdrawal: Box<Account<'info, WithdrawalRequest>>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        seeds = [b"mint", mint.key().as_ref()],
        bump = mint_bridge.bump,
        has_one = vault
    )]
    pub mint_bridge: Box<Account<'info, MintBridge>>,
    #[account(mut)]
    pub vault: Box<Account<'info, TokenAccount>>,
    /// CHECK: the requester's wallet, pinned by `has_one = user` above.
    pub user: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = attester,
        associated_token::mint = mint,
        associated_token::authority = user,
    )]
    pub user_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(btc_txid: [u8; 32], vout: u32, amount: u64)]
pub struct ReleaseDeposit<'info> {
    #[account(mut)]
    pub attester: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = attester @ BridgeError::Unauthorized)]
    pub config: Account<'info, BridgeConfig>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        seeds = [b"mint", mint.key().as_ref()],
        bump = mint_bridge.bump,
        has_one = vault,
        has_one = mint
    )]
    pub mint_bridge: Box<Account<'info, MintBridge>>,
    #[account(mut)]
    pub vault: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = attester,
        space = 8 + DepositReceipt::INIT_SPACE,
        seeds = [b"deposit", mint.key().as_ref(), &btc_txid, &vout.to_le_bytes()],
        bump
    )]
    pub receipt: Box<Account<'info, DepositReceipt>>,
    /// CHECK: any wallet; tokens go to its canonical ATA (created if needed).
    pub recipient: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = attester,
        associated_token::mint = mint,
        associated_token::authority = recipient,
    )]
    pub recipient_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

// ---------------------------------------------------------------------------
// Events & errors
// ---------------------------------------------------------------------------
#[event]
pub struct MintRegistered {
    pub mint: Pubkey,
    pub vault: Pubkey,
}
#[event]
pub struct RuneEnabled {
    pub mint: Pubkey,
    pub rune_block: u64,
    pub rune_tx: u32,
}
#[event]
pub struct WithdrawalRequested {
    pub mint: Pubkey,
    pub seq: u64,
    pub request: Pubkey,
    pub user: Pubkey,
    pub amount: u64,
    pub btc_witness_program: [u8; 32],
}
#[event]
pub struct WithdrawalSent {
    pub mint: Pubkey,
    pub seq: u64,
    pub btc_txid: [u8; 32],
}
#[event]
pub struct WithdrawalRefunded {
    pub mint: Pubkey,
    pub seq: u64,
    pub amount: u64,
}
#[event]
pub struct DepositReleased {
    pub mint: Pubkey,
    pub btc_txid: [u8; 32],
    pub vout: u32,
    pub amount: u64,
    pub recipient: Pubkey,
}
#[event]
pub struct PausedChanged {
    pub paused: bool,
    pub by: Pubkey,
}
#[event]
pub struct ConfigUpdated {
    pub attester: Pubkey,
    pub guardian: Pubkey,
    pub fee_receiver: Pubkey,
    pub withdrawal_fee_lamports: u64,
}

#[error_code]
pub enum BridgeError {
    #[msg("Not authorized")]
    Unauthorized,
    #[msg("Bridge is paused")]
    Paused,
    #[msg("Rune not enabled for this coin (etch not final yet)")]
    NotEnabled,
    #[msg("Rune already enabled")]
    AlreadyEnabled,
    #[msg("Invalid Rune ID")]
    InvalidRuneId,
    #[msg("Mint is not an Etchy coin (6 decimals, supply <= 10^15, no mint/freeze authority)")]
    NotAnEtchyCoin,
    #[msg("Amount below the minimum")]
    AmountTooSmall,
    #[msg("Invalid Bitcoin Taproot address")]
    InvalidBtcAddress,
    #[msg("Invalid Bitcoin txid")]
    InvalidBtcTxid,
    #[msg("Withdrawal is not pending")]
    NotPending,
    #[msg("Withdrawal is still pending")]
    StillPending,
    #[msg("Daily release cap exceeded")]
    DailyCapExceeded,
    #[msg("Invalid limits")]
    InvalidLimits,
    #[msg("Fee too high")]
    FeeTooHigh,
    #[msg("Fee receiver differs from config")]
    InvalidFeeReceiver,
    #[msg("Refund must go to the requester's own wallet")]
    WrongRefundAccount,
    #[msg("Arithmetic overflow")]
    Overflow,
}
