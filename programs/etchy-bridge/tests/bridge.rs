//! M8 Etchy bridge — LiteSVM tests (run in Docker: see docs/M8_RUNE_BRIDGE.md).
use anchor_lang::prelude::{Clock, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use solana_instruction_error::InstructionError;
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use etchy_bridge::{
    accounts as acc, instruction as ix, BridgeConfig, BridgeError, DepositReceipt,
    InitializeArgs, MintBridge, MintLimits, UpdateConfigArgs, WithdrawalRequest,
    WithdrawalStatus, ETCHY_SUPPLY,
};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_error::TransactionError;
use std::str::FromStr;

const TOKEN: Pubkey = anchor_spl::token::ID;
const ATA: Pubkey = anchor_spl::associated_token::ID;
const SYSTEM: Pubkey = anchor_lang::system_program::ID;
const FEE: u64 = 2_000_000; // 0.002 SOL per Bridge-to-Rune
const MIN: u64 = 1_000_000; // 1 token
const CAP: u64 = 50_000_000_000; // 50k tokens/day
const USER_BAL: u64 = 1_000_000_000_000; // 1M tokens
const NOW: i64 = 1_800_000_000;

fn loader_v3() -> Pubkey {
    Pubkey::from_str("BPFLoaderUpgradeab1e11111111111111111111111").unwrap()
}

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    guardian: Keypair,
    attester: Keypair,
    fee_receiver: Pubkey,
    user: Keypair,
    mint: Pubkey,
    user_token: Pubkey,
}

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &etchy_bridge::ID).0
}
fn config_pda() -> Pubkey {
    pda(&[b"config"])
}
fn mint_bridge_pda(mint: &Pubkey) -> Pubkey {
    pda(&[b"mint", mint.as_ref()])
}
fn vault_pda(mint: &Pubkey) -> Pubkey {
    pda(&[b"vault", mint.as_ref()])
}
/// The main test user (fixed key so request PDAs are easy to derive).
fn default_user() -> Keypair {
    Keypair::new_from_array([42u8; 32])
}
fn withdrawal_pda_of(mint: &Pubkey, user: &Pubkey, request_id: u64) -> Pubkey {
    pda(&[b"withdrawal", mint.as_ref(), user.as_ref(), &request_id.to_le_bytes()])
}
fn withdrawal_pda(mint: &Pubkey, request_id: u64) -> Pubkey {
    withdrawal_pda_of(mint, &default_user().pubkey(), request_id)
}
fn receipt_pda(mint: &Pubkey, txid: &[u8; 32], vout: u32) -> Pubkey {
    pda(&[b"deposit", mint.as_ref(), txid, &vout.to_le_bytes()])
}
fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[owner.as_ref(), TOKEN.as_ref(), mint.as_ref()], &ATA).0
}

// --- raw SPL Token account layouts (Mint 82 bytes, Account 165 bytes) ---
fn coption_key(out: &mut Vec<u8>, k: Option<Pubkey>) {
    match k {
        Some(k) => {
            out.extend_from_slice(&1u32.to_le_bytes());
            out.extend_from_slice(k.as_ref());
        }
        None => {
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&[0u8; 32]);
        }
    }
}
fn mint_bytes(supply: u64, decimals: u8, mint_auth: Option<Pubkey>, freeze: Option<Pubkey>) -> Vec<u8> {
    let mut d = Vec::with_capacity(82);
    coption_key(&mut d, mint_auth);
    d.extend_from_slice(&supply.to_le_bytes());
    d.push(decimals);
    d.push(1);
    coption_key(&mut d, freeze);
    assert_eq!(d.len(), 82);
    d
}
fn token_bytes(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = Vec::with_capacity(165);
    d.extend_from_slice(mint.as_ref());
    d.extend_from_slice(owner.as_ref());
    d.extend_from_slice(&amount.to_le_bytes());
    coption_key(&mut d, None);
    d.push(1); // Initialized
    d.extend_from_slice(&0u32.to_le_bytes());
    d.extend_from_slice(&0u64.to_le_bytes());
    d.extend_from_slice(&0u64.to_le_bytes());
    coption_key(&mut d, None);
    assert_eq!(d.len(), 165);
    d
}
fn put(svm: &mut LiteSVM, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(key, Account { lamports, data, owner, executable: false, rent_epoch: 0 })
        .unwrap();
}
fn new_mint(svm: &mut LiteSVM, supply: u64, decimals: u8, auth: Option<Pubkey>, freeze: Option<Pubkey>) -> Pubkey {
    let k = Pubkey::new_unique();
    put(svm, k, TOKEN, mint_bytes(supply, decimals, auth, freeze));
    k
}
fn new_token_account(svm: &mut LiteSVM, mint: &Pubkey, owner: &Pubkey, amount: u64) -> Pubkey {
    let k = ata(owner, mint);
    put(svm, k, TOKEN, token_bytes(mint, owner, amount));
    k
}
fn balance(svm: &LiteSVM, token_account: &Pubkey) -> u64 {
    let a = svm.get_account(token_account).expect("token account");
    u64::from_le_bytes(a.data[64..72].try_into().unwrap())
}
fn read<T: AccountDeserialize>(svm: &LiteSVM, key: &Pubkey) -> T {
    let a = svm.get_account(key).expect("account");
    T::try_deserialize(&mut a.data.as_slice()).unwrap()
}

fn send(svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), TransactionError> {
    svm.expire_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&signers[0].pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
    svm.send_transaction(tx).map(|_| ()).map_err(|e| e.err)
}
fn custom(code: u32) -> TransactionError {
    TransactionError::InstructionError(0, InstructionError::Custom(code))
}
fn bridge_err(e: BridgeError) -> TransactionError {
    custom(6000 + e as u32)
}
/// Anchor built-in error codes used by the constraints.
const ANCHOR_CONSTRAINT_HAS_ONE: u32 = 2001;
const ANCHOR_TOKEN_MINT: u32 = 2014;
const ANCHOR_TOKEN_OWNER: u32 = 2015;
const SYSTEM_ACCOUNT_IN_USE: u32 = 0;

fn ix_of(data: impl InstructionData, accounts: impl ToAccountMetas) -> Instruction {
    Instruction { program_id: etchy_bridge::ID, accounts: accounts.to_account_metas(None), data: data.data() }
}

fn set_time(svm: &mut LiteSVM, unix: i64) {
    let mut c = svm.get_sysvar::<Clock>();
    c.unix_timestamp = unix;
    svm.set_sysvar(&c);
}

/// Deploy the program with `upgrade_authority` (LiteSVM deploys with none).
fn deploy(svm: &mut LiteSVM, upgrade_authority: &Pubkey) {
    let so = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/etchy_bridge.so"))
        .expect("build first: anchor build");
    svm.add_program(etchy_bridge::ID, &so).unwrap();
    let pd = Pubkey::find_program_address(&[etchy_bridge::ID.as_ref()], &loader_v3()).0;
    let mut a = svm.get_account(&pd).unwrap();
    // bincode UpgradeableLoaderState::ProgramData { slot: u64, upgrade_authority_address: Option<Pubkey> }
    assert_eq!(u32::from_le_bytes(a.data[0..4].try_into().unwrap()), 3);
    a.data[12] = 1;
    a.data[13..45].copy_from_slice(upgrade_authority.as_ref());
    svm.set_account(pd, a).unwrap();
}

fn initialize_ix(env_admin: &Pubkey, payer: &Pubkey, args: InitializeArgs) -> Instruction {
    let _ = env_admin;
    let program_data = Pubkey::find_program_address(&[etchy_bridge::ID.as_ref()], &loader_v3()).0;
    ix_of(
        ix::Initialize { args },
        acc::Initialize {
            payer: *payer,
            config: config_pda(),
            program: etchy_bridge::ID,
            program_data,
            system_program: SYSTEM,
        },
    )
}

fn register_ix(admin: &Pubkey, mint: &Pubkey, limits: MintLimits) -> Instruction {
    ix_of(
        ix::RegisterMint { limits },
        acc::RegisterMint {
            admin: *admin,
            config: config_pda(),
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            vault: vault_pda(mint),
            token_program: TOKEN,
            system_program: SYSTEM,
        },
    )
}

fn enable_ix(admin: &Pubkey, mint: &Pubkey, block: u64, tx: u32) -> Instruction {
    ix_of(
        ix::EnableRune { rune_block: block, rune_tx: tx },
        acc::AdminMint { admin: *admin, config: config_pda(), mint_bridge: mint_bridge_pda(mint) },
    )
}

fn bridge_ix(env: &Env, user: &Pubkey, user_token: &Pubkey, fee_receiver: &Pubkey, rid: u64, amount: u64, wp: [u8; 32]) -> Instruction {
    ix_of(
        ix::BridgeToRune { request_id: rid, amount, btc_witness_program: wp },
        acc::BridgeToRune {
            user: *user,
            config: config_pda(),
            fee_receiver: *fee_receiver,
            mint_bridge: mint_bridge_pda(&env.mint),
            user_token: *user_token,
            vault: vault_pda(&env.mint),
            withdrawal: withdrawal_pda_of(&env.mint, user, rid),
            token_program: TOKEN,
            system_program: SYSTEM,
        },
    )
}

fn sent_ix(attester: &Pubkey, mint: &Pubkey, rid: u64, txid: [u8; 32]) -> Instruction {
    sent_ix_for(attester, &withdrawal_pda(mint, rid), txid)
}
fn sent_ix_for(attester: &Pubkey, withdrawal: &Pubkey, txid: [u8; 32]) -> Instruction {
    ix_of(
        ix::MarkWithdrawalSent { btc_txid: txid },
        acc::AttestWithdrawal { attester: *attester, config: config_pda(), withdrawal: *withdrawal },
    )
}
fn close_ix(user: &Pubkey, withdrawal: &Pubkey) -> Instruction {
    ix_of(ix::CloseWithdrawal {}, acc::CloseWithdrawal { user: *user, withdrawal: *withdrawal })
}

/// Refund of the default user's request (tokens go to the user's own ATA).
fn refund_ix(attester: &Pubkey, mint: &Pubkey, rid: u64, user_token: &Pubkey) -> Instruction {
    refund_ix_to(attester, mint, rid, &default_user().pubkey(), user_token)
}
fn refund_ix_to(attester: &Pubkey, mint: &Pubkey, rid: u64, wallet: &Pubkey, wallet_token: &Pubkey) -> Instruction {
    ix_of(
        ix::RefundWithdrawal {},
        acc::RefundWithdrawal {
            attester: *attester,
            config: config_pda(),
            withdrawal: withdrawal_pda(mint, rid),
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            vault: vault_pda(mint),
            user: *wallet,
            user_token: *wallet_token,
            token_program: TOKEN,
            associated_token_program: ATA,
            system_program: SYSTEM,
        },
    )
}

fn release_ix(attester: &Pubkey, mint: &Pubkey, txid: [u8; 32], vout: u32, amount: u64, recipient: &Pubkey) -> Instruction {
    ix_of(
        ix::ReleaseDeposit { btc_txid: txid, vout, amount },
        acc::ReleaseDeposit {
            attester: *attester,
            config: config_pda(),
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            vault: vault_pda(mint),
            receipt: receipt_pda(mint, &txid, vout),
            recipient: *recipient,
            recipient_token: ata(recipient, mint),
            token_program: TOKEN,
            associated_token_program: ATA,
            system_program: SYSTEM,
        },
    )
}

fn pause_ix(authority: &Pubkey) -> Instruction {
    ix_of(ix::Pause {}, acc::Pause { authority: *authority, config: config_pda() })
}
fn unpause_ix(admin: &Pubkey) -> Instruction {
    ix_of(ix::Unpause {}, acc::AdminConfig { admin: *admin, config: config_pda() })
}

/// Fresh chain: program deployed by `admin`, config initialized, one Etchy
/// coin registered (NOT yet enabled), user holding USER_BAL tokens.
fn setup() -> Env {
    let mut svm = LiteSVM::new();
    set_time(&mut svm, NOW);
    let admin = Keypair::new();
    let guardian = Keypair::new();
    let attester = Keypair::new();
    let user = default_user();
    let fee_receiver = Pubkey::new_unique();
    for k in [&admin, &guardian, &attester, &user] {
        svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
    }
    svm.airdrop(&fee_receiver, 1_000_000_000).unwrap();
    deploy(&mut svm, &admin.pubkey());
    let mint = new_mint(&mut svm, ETCHY_SUPPLY, 6, None, None);
    let user_token = new_token_account(&mut svm, &mint, &user.pubkey(), USER_BAL);
    let args = InitializeArgs {
        admin: admin.pubkey(),
        guardian: guardian.pubkey(),
        attester: attester.pubkey(),
        fee_receiver,
        withdrawal_fee_lamports: FEE,
    };
    send(&mut svm, &[initialize_ix(&admin.pubkey(), &admin.pubkey(), args)], &[&admin]).unwrap();
    send(
        &mut svm,
        &[register_ix(&admin.pubkey(), &mint, MintLimits { min_withdrawal: MIN, daily_release_cap: CAP })],
        &[&admin],
    )
    .unwrap();
    Env { svm, admin, guardian, attester, fee_receiver, user, mint, user_token }
}

fn enabled() -> Env {
    let mut env = setup();
    let admin = env.admin.insecure_clone();
    send(&mut env.svm, &[enable_ix(&admin.pubkey(), &env.mint, 970_711, 199)], &[&admin]).unwrap();
    env
}

fn wp(n: u8) -> [u8; 32] {
    [n; 32]
}

fn lock(env: &mut Env, rid: u64, amount: u64) {
    let user = env.user.insecure_clone();
    let i = bridge_ix(env, &user.pubkey(), &env.user_token.clone(), &env.fee_receiver.clone(), rid, amount, wp(7));
    send(&mut env.svm, &[i], &[&user]).unwrap();
}

fn assert_invariant(env: &Env) {
    let b: MintBridge = read(&env.svm, &mint_bridge_pda(&env.mint));
    let vault = balance(&env.svm, &vault_pda(&env.mint));
    assert_eq!(vault, b.total_locked - b.total_released - b.total_refunded, "vault == locked - released - refunded");
}

// ---------------------------------------------------------------------------

#[test]
fn initialize_only_by_upgrade_authority_and_once() {
    let mut svm = LiteSVM::new();
    let admin = Keypair::new();
    let stranger = Keypair::new();
    svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    deploy(&mut svm, &admin.pubkey());
    let args = || InitializeArgs {
        admin: admin.pubkey(),
        guardian: admin.pubkey(),
        attester: admin.pubkey(),
        fee_receiver: admin.pubkey(),
        withdrawal_fee_lamports: FEE,
    };
    let e = send(&mut svm, &[initialize_ix(&admin.pubkey(), &stranger.pubkey(), args())], &[&stranger]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let mut bad = args();
    bad.withdrawal_fee_lamports = 2_000_000_000;
    let e = send(&mut svm, &[initialize_ix(&admin.pubkey(), &admin.pubkey(), bad)], &[&admin]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::FeeTooHigh));
    send(&mut svm, &[initialize_ix(&admin.pubkey(), &admin.pubkey(), args())], &[&admin]).unwrap();
    let c: BridgeConfig = read(&svm, &config_pda());
    assert_eq!(c.admin, admin.pubkey());
    assert!(!c.paused);
    let e = send(&mut svm, &[initialize_ix(&admin.pubkey(), &admin.pubkey(), args())], &[&admin]).unwrap_err();
    assert_eq!(e, custom(SYSTEM_ACCOUNT_IN_USE));
}

#[test]
fn register_accepts_only_real_etchy_coins_by_admin() {
    let mut env = setup();
    let admin = env.admin.insecure_clone();
    let limits = || MintLimits { min_withdrawal: MIN, daily_release_cap: CAP };
    let b: MintBridge = read(&env.svm, &mint_bridge_pda(&env.mint));
    assert!(!b.enabled && b.vault == vault_pda(&env.mint));
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 0);

    let someone = Pubkey::new_unique();
    let bad_mints = [
        new_mint(&mut env.svm, ETCHY_SUPPLY, 6, Some(someone), None), // can still mint
        new_mint(&mut env.svm, ETCHY_SUPPLY, 6, None, Some(someone)), // can freeze
        new_mint(&mut env.svm, ETCHY_SUPPLY, 9, None, None),          // wrong decimals
        new_mint(&mut env.svm, ETCHY_SUPPLY + 1, 6, None, None),      // more than the Rune premine
        new_mint(&mut env.svm, 0, 6, None, None),                     // empty
    ];
    for m in bad_mints {
        let e = send(&mut env.svm, &[register_ix(&admin.pubkey(), &m, limits())], &[&admin]).unwrap_err();
        assert_eq!(e, bridge_err(BridgeError::NotAnEtchyCoin));
    }
    let good = new_mint(&mut env.svm, ETCHY_SUPPLY, 6, None, None);
    let stranger = env.user.insecure_clone();
    let e = send(&mut env.svm, &[register_ix(&stranger.pubkey(), &good, limits())], &[&stranger]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let e = send(
        &mut env.svm,
        &[register_ix(&admin.pubkey(), &good, MintLimits { min_withdrawal: 0, daily_release_cap: CAP })],
        &[&admin],
    )
    .unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::InvalidLimits));
    send(&mut env.svm, &[register_ix(&admin.pubkey(), &good, limits())], &[&admin]).unwrap();
    // A coin can only be registered once.
    let e = send(&mut env.svm, &[register_ix(&admin.pubkey(), &good, limits())], &[&admin]).unwrap_err();
    assert_eq!(e, custom(SYSTEM_ACCOUNT_IN_USE));
    // A coin whose holders burned some tokens is still fully backed.
    let burned = new_mint(&mut env.svm, ETCHY_SUPPLY - 123, 6, None, None);
    send(&mut env.svm, &[register_ix(&admin.pubkey(), &burned, limits())], &[&admin]).unwrap();
}

#[test]
fn many_users_can_bridge_at_once_without_colliding() {
    let mut env = enabled();
    let user = env.user.insecure_clone();
    let other = Keypair::new();
    env.svm.airdrop(&other.pubkey(), 1_000_000_000).unwrap();
    let other_token = new_token_account(&mut env.svm, &env.mint.clone(), &other.pubkey(), USER_BAL);
    let (ut, fr) = (env.user_token, env.fee_receiver);
    // Both pick the same request id: they are still separate requests.
    let a = bridge_ix(&env, &user.pubkey(), &ut, &fr, 77, 2_000_000, wp(1));
    let b = bridge_ix(&env, &other.pubkey(), &other_token, &fr, 77, 3_000_000, wp(2));
    send(&mut env.svm, &[a.clone()], &[&user]).unwrap();
    send(&mut env.svm, &[b], &[&other]).unwrap();
    // Re-using your own id is refused (no double-lock by accident).
    assert_eq!(send(&mut env.svm, &[a], &[&user]).unwrap_err(), custom(SYSTEM_ACCOUNT_IN_USE));
    let wa: WithdrawalRequest = read(&env.svm, &withdrawal_pda(&env.mint, 77));
    let wb_key = withdrawal_pda_of(&env.mint, &other.pubkey(), 77);
    let wb: WithdrawalRequest = read(&env.svm, &wb_key);
    assert_eq!((wa.seq, wa.amount, wa.btc_witness_program), (0, 2_000_000, wp(1)));
    assert_eq!((wb.seq, wb.amount, wb.btc_witness_program, wb.user), (1, 3_000_000, wp(2), other.pubkey()));
    let attester = env.attester.insecure_clone();
    send(&mut env.svm, &[sent_ix_for(&attester.pubkey(), &wb_key, [9; 32])], &[&attester]).unwrap();
    assert_invariant(&env);
}

#[test]
fn finished_requests_can_be_closed_by_their_owner_for_rent() {
    let mut env = enabled();
    lock(&mut env, 5, 2_000_000);
    let (user, attester) = (env.user.insecure_clone(), env.attester.insecure_clone());
    let w = withdrawal_pda(&env.mint, 5);
    let e = send(&mut env.svm, &[close_ix(&user.pubkey(), &w)], &[&user]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::StillPending));
    send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 5, [3; 32])], &[&attester]).unwrap();
    let e = send(&mut env.svm, &[close_ix(&attester.pubkey(), &w)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let rent = env.svm.get_balance(&w).unwrap();
    let before = env.svm.get_balance(&user.pubkey()).unwrap();
    send(&mut env.svm, &[close_ix(&user.pubkey(), &w)], &[&user]).unwrap();
    assert_eq!(env.svm.get_balance(&user.pubkey()).unwrap(), before + rent - 5_000);
    assert!(env.svm.get_account(&w).map_or(true, |a| a.lamports == 0 && a.data.is_empty()));
    assert_invariant(&env);
}

#[test]
fn nothing_moves_until_rune_is_final_and_rune_id_is_permanent() {
    let mut env = setup();
    let (admin, user, attester) = (env.admin.insecure_clone(), env.user.insecure_clone(), env.attester.insecure_clone());
    let i = bridge_ix(&env, &user.pubkey(), &env.user_token, &env.fee_receiver, 0, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::NotEnabled));
    let r = release_ix(&attester.pubkey(), &env.mint, [9; 32], 0, MIN, &user.pubkey());
    assert_eq!(send(&mut env.svm, &[r], &[&attester]).unwrap_err(), bridge_err(BridgeError::NotEnabled));

    let e = send(&mut env.svm, &[enable_ix(&user.pubkey(), &env.mint, 970_711, 199)], &[&user]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let e = send(&mut env.svm, &[enable_ix(&admin.pubkey(), &env.mint, 0, 1)], &[&admin]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::InvalidRuneId));
    send(&mut env.svm, &[enable_ix(&admin.pubkey(), &env.mint, 970_711, 199)], &[&admin]).unwrap();
    let b: MintBridge = read(&env.svm, &mint_bridge_pda(&env.mint));
    assert!(b.enabled && b.rune_block == 970_711 && b.rune_tx == 199);
    let e = send(&mut env.svm, &[enable_ix(&admin.pubkey(), &env.mint, 1, 1)], &[&admin]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::AlreadyEnabled));
}

#[test]
fn bridge_to_rune_locks_tokens_charges_fee_and_records_request() {
    let mut env = enabled();
    let fee_before = env.svm.get_balance(&env.fee_receiver).unwrap();
    lock(&mut env, 0, 250_000_000);
    lock(&mut env, 1, 1_000_000);
    assert_eq!(balance(&env.svm, &env.user_token), USER_BAL - 251_000_000);
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 251_000_000);
    assert_eq!(env.svm.get_balance(&env.fee_receiver).unwrap(), fee_before + 2 * FEE);
    let w: WithdrawalRequest = read(&env.svm, &withdrawal_pda(&env.mint, 0));
    assert_eq!((w.seq, w.amount, w.status), (0, 250_000_000, WithdrawalStatus::Pending));
    assert_eq!((w.user, w.user_token, w.btc_witness_program), (env.user.pubkey(), env.user_token, wp(7)));
    assert_eq!(w.requested_at, NOW);
    let b: MintBridge = read(&env.svm, &mint_bridge_pda(&env.mint));
    assert_eq!((b.withdrawal_seq, b.total_locked), (2, 251_000_000));

    // Attester records the Bitcoin send; it is final.
    let (attester, user) = (env.attester.insecure_clone(), env.user.insecure_clone());
    let e = send(&mut env.svm, &[sent_ix(&user.pubkey(), &env.mint, 0, [5; 32])], &[&user]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let e = send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 0, [0; 32])], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::InvalidBtcTxid));
    send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 0, [5; 32])], &[&attester]).unwrap();
    let w: WithdrawalRequest = read(&env.svm, &withdrawal_pda(&env.mint, 0));
    assert_eq!((w.status, w.btc_txid), (WithdrawalStatus::Sent, [5; 32]));
    let e = send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 0, [6; 32])], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::NotPending));
    let ut = env.user_token;
    let e = send(&mut env.svm, &[refund_ix(&attester.pubkey(), &env.mint, 0, &ut)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::NotPending), "a sent withdrawal can never be refunded");
    assert_invariant(&env);
}

#[test]
fn bridge_to_rune_rejects_bad_input() {
    let mut env = enabled();
    let user = env.user.insecure_clone();
    let (ut, fr) = (env.user_token, env.fee_receiver);
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 0, MIN - 1, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::AmountTooSmall));
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 0, MIN, [0; 32]);
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::InvalidBtcAddress));
    let thief = Pubkey::new_unique();
    let i = bridge_ix(&env, &user.pubkey(), &ut, &thief, 0, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::InvalidFeeReceiver));
    // Someone else's token account cannot be spent.
    let victim = Pubkey::new_unique();
    let victim_token = new_token_account(&mut env.svm, &env.mint.clone(), &victim, USER_BAL);
    let i = bridge_ix(&env, &user.pubkey(), &victim_token, &fr, 0, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), custom(ANCHOR_TOKEN_OWNER));
    // A different coin cannot be deposited into this coin's vault.
    let other = new_mint(&mut env.svm, ETCHY_SUPPLY, 6, None, None);
    let other_token = new_token_account(&mut env.svm, &other, &user.pubkey(), USER_BAL);
    let i = bridge_ix(&env, &user.pubkey(), &other_token, &fr, 0, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), custom(ANCHOR_TOKEN_MINT));
    // More than the balance fails inside SPL Token.
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 0, USER_BAL + 1, wp(7));
    assert!(send(&mut env.svm, &[i], &[&user]).is_err());
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 0);
}

#[test]
fn refund_only_to_the_original_account_and_only_once() {
    let mut env = enabled();
    lock(&mut env, 0, 40_000_000);
    let attester = env.attester.insecure_clone();
    let stranger = Pubkey::new_unique();
    let stranger_token = ata(&stranger, &env.mint);
    let e = send(&mut env.svm, &[refund_ix_to(&attester.pubkey(), &env.mint, 0, &stranger, &stranger_token)], &[&attester])
        .unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::WrongRefundAccount));
    let user = env.user.insecure_clone();
    let ut = env.user_token;
    let e = send(&mut env.svm, &[refund_ix(&user.pubkey(), &env.mint, 0, &ut)], &[&user]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    send(&mut env.svm, &[refund_ix(&attester.pubkey(), &env.mint, 0, &ut)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ut), USER_BAL);
    let w: WithdrawalRequest = read(&env.svm, &withdrawal_pda(&env.mint, 0));
    assert_eq!(w.status, WithdrawalStatus::Refunded);
    let e = send(&mut env.svm, &[refund_ix(&attester.pubkey(), &env.mint, 0, &ut)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::NotPending));
    let e = send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 0, [5; 32])], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::NotPending), "a refunded withdrawal can never be marked sent");
    assert_invariant(&env);
}

#[test]
fn refund_reaches_the_user_even_after_they_closed_their_token_account() {
    let mut env = enabled();
    // The user bridges everything, then closes (deletes) their empty token account.
    lock(&mut env, 0, USER_BAL);
    let ut = env.user_token;
    assert_eq!(balance(&env.svm, &ut), 0);
    env.svm
        .set_account(ut, Account { lamports: 0, data: vec![], owner: SYSTEM, executable: false, rent_epoch: 0 })
        .unwrap();
    let attester = env.attester.insecure_clone();
    send(&mut env.svm, &[refund_ix(&attester.pubkey(), &env.mint, 0, &ut)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ut), USER_BAL);
    assert_invariant(&env);
}

#[test]
fn release_deposit_once_per_bitcoin_outpoint() {
    let mut env = enabled();
    lock(&mut env, 0, 10_000_000_000);
    let attester = env.attester.insecure_clone();
    let alice = Pubkey::new_unique(); // has no token account yet
    let txid = [0xab; 32];
    send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, txid, 1, 3_000_000_000, &alice)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ata(&alice, &env.mint)), 3_000_000_000);
    let r: DepositReceipt = read(&env.svm, &receipt_pda(&env.mint, &txid, 1));
    assert_eq!((r.amount, r.recipient, r.vout, r.btc_txid), (3_000_000_000, alice, 1, txid));

    // Same outpoint again (even to another wallet / amount) is impossible.
    let bob = Pubkey::new_unique();
    let e = send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, txid, 1, 1, &bob)], &[&attester]).unwrap_err();
    assert_eq!(e, custom(SYSTEM_ACCOUNT_IN_USE));
    // Another output of the same Bitcoin tx is a different deposit.
    send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, txid, 2, 1_000_000_000, &alice)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ata(&alice, &env.mint)), 4_000_000_000);

    let user = env.user.insecure_clone();
    let e = send(&mut env.svm, &[release_ix(&user.pubkey(), &env.mint, [1; 32], 0, 1, &user.pubkey())], &[&user]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let e = send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [0; 32], 0, 1, &alice)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::InvalidBtcTxid));
    let e = send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [1; 32], 0, 0, &alice)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::AmountTooSmall));
    // Never more than the vault holds (only Rune that left custody can come back).
    let e = send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [1; 32], 0, 6_000_000_001, &alice)], &[&attester]);
    assert!(e.is_err());
    assert_invariant(&env);
}

#[test]
fn daily_release_cap_resets_each_utc_day() {
    let mut env = enabled();
    lock(&mut env, 0, 2 * CAP);
    let attester = env.attester.insecure_clone();
    let alice = Pubkey::new_unique();
    send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [1; 32], 0, CAP - 1, &alice)], &[&attester]).unwrap();
    let e = send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [1; 32], 1, 2, &alice)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::DailyCapExceeded));
    send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [1; 32], 1, 1, &alice)], &[&attester]).unwrap();
    set_time(&mut env.svm, NOW + 86_400);
    send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [1; 32], 2, CAP, &alice)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ata(&alice, &env.mint)), 2 * CAP);
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 0);
    assert_invariant(&env);
}

#[test]
fn pause_by_guardian_unpause_only_by_admin() {
    let mut env = enabled();
    lock(&mut env, 0, 5_000_000);
    let (admin, guardian, attester, user) = (
        env.admin.insecure_clone(),
        env.guardian.insecure_clone(),
        env.attester.insecure_clone(),
        env.user.insecure_clone(),
    );
    let e = send(&mut env.svm, &[pause_ix(&user.pubkey())], &[&user]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    send(&mut env.svm, &[pause_ix(&guardian.pubkey())], &[&guardian]).unwrap();
    let (ut, fr) = (env.user_token, env.fee_receiver);
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 1, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::Paused));
    let r = release_ix(&attester.pubkey(), &env.mint, [3; 32], 0, MIN, &user.pubkey());
    assert_eq!(send(&mut env.svm, &[r], &[&attester]).unwrap_err(), bridge_err(BridgeError::Paused));
    // Pending users can still be made whole while paused.
    send(&mut env.svm, &[refund_ix(&attester.pubkey(), &env.mint, 0, &ut)], &[&attester]).unwrap();
    let e = send(&mut env.svm, &[unpause_ix(&guardian.pubkey())], &[&guardian]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    send(&mut env.svm, &[unpause_ix(&admin.pubkey())], &[&admin]).unwrap();
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 1, MIN, wp(7));
    send(&mut env.svm, &[i], &[&user]).unwrap();

    // Per-coin pause.
    let set = |p: bool| {
        ix_of(
            ix::SetMintPaused { paused: p },
            acc::AdminMint { admin: admin.pubkey(), config: config_pda(), mint_bridge: mint_bridge_pda(&env.mint) },
        )
    };
    send(&mut env.svm, &[set(true)], &[&admin]).unwrap();
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 2, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i.clone()], &[&user]).unwrap_err(), bridge_err(BridgeError::Paused));
    send(&mut env.svm, &[set(false)], &[&admin]).unwrap();
    send(&mut env.svm, &[i], &[&user]).unwrap();
    assert_invariant(&env);
}

#[test]
fn admin_rotation_is_two_step_and_attester_rotation_takes_effect() {
    let mut env = enabled();
    lock(&mut env, 0, 10_000_000);
    let (admin, attester) = (env.admin.insecure_clone(), env.attester.insecure_clone());
    let new_attester = Keypair::new();
    env.svm.airdrop(&new_attester.pubkey(), 1_000_000_000).unwrap();
    let upd = |a: &Keypair, att: Pubkey, fee: u64| {
        ix_of(
            ix::UpdateConfig {
                args: UpdateConfigArgs {
                    guardian: env.guardian.pubkey(),
                    attester: att,
                    fee_receiver: env.fee_receiver,
                    withdrawal_fee_lamports: fee,
                },
            },
            acc::AdminConfig { admin: a.pubkey(), config: config_pda() },
        )
    };
    let e = send(&mut env.svm, &[upd(&attester, attester.pubkey(), FEE)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let e = send(&mut env.svm, &[upd(&admin, new_attester.pubkey(), 1_000_000_001)], &[&admin]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::FeeTooHigh));
    send(&mut env.svm, &[upd(&admin, new_attester.pubkey(), FEE)], &[&admin]).unwrap();
    let alice = Pubkey::new_unique();
    let e = send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [4; 32], 0, MIN, &alice)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized), "old attester is locked out");
    send(&mut env.svm, &[release_ix(&new_attester.pubkey(), &env.mint, [4; 32], 0, MIN, &alice)], &[&new_attester]).unwrap();

    let new_admin = Keypair::new();
    env.svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    let propose = ix_of(ix::ProposeAdmin { new_admin: new_admin.pubkey() }, acc::AdminConfig { admin: admin.pubkey(), config: config_pda() });
    send(&mut env.svm, &[propose], &[&admin]).unwrap();
    let accept = |k: &Keypair| ix_of(ix::AcceptAdmin {}, acc::AcceptAdmin { new_admin: k.pubkey(), config: config_pda() });
    let e = send(&mut env.svm, &[accept(&attester)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    send(&mut env.svm, &[accept(&new_admin)], &[&new_admin]).unwrap();
    let c: BridgeConfig = read(&env.svm, &config_pda());
    assert_eq!((c.admin, c.pending_admin), (new_admin.pubkey(), Pubkey::default()));
    let e = send(&mut env.svm, &[unpause_ix(&admin.pubkey())], &[&admin]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized), "old admin is locked out");
    let _ = ANCHOR_CONSTRAINT_HAS_ONE;
    assert_invariant(&env);
}

#[test]
fn full_round_trip_keeps_rune_custody_equal_to_circulating_supply() {
    // Model: custody starts with the whole premine (all tokens circulate on Solana).
    let mut env = enabled();
    let mut custody: u64 = ETCHY_SUPPLY;
    let circulating = |env: &Env| ETCHY_SUPPLY - balance(&env.svm, &vault_pda(&env.mint));
    assert_eq!(custody, circulating(&env));
    let attester = env.attester.insecure_clone();
    // 1) user bridges 300 tokens to Rune; attester sends 300 Rune out of custody.
    lock(&mut env, 0, 300_000_000);
    send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 0, [8; 32])], &[&attester]).unwrap();
    custody -= 300_000_000;
    assert_eq!(custody, circulating(&env));
    // 2) user bridges 50 more; Bitcoin send fails -> refund.
    lock(&mut env, 1, 50_000_000);
    let ut = env.user_token;
    send(&mut env.svm, &[refund_ix(&attester.pubkey(), &env.mint, 1, &ut)], &[&attester]).unwrap();
    assert_eq!(custody, circulating(&env));
    // 3) someone deposits 120 Rune to custody on Bitcoin -> 120 tokens released.
    custody += 120_000_000;
    let bob = Pubkey::new_unique();
    send(&mut env.svm, &[release_ix(&attester.pubkey(), &env.mint, [2; 32], 0, 120_000_000, &bob)], &[&attester]).unwrap();
    assert_eq!(custody, circulating(&env));
    assert_eq!(balance(&env.svm, &ata(&bob, &env.mint)), 120_000_000);
    assert_invariant(&env);
}
