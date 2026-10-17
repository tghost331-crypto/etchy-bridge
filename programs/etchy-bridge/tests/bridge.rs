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

// ===========================================================================
// Token-2022 coins (metadata in the mint: MetadataPointer + TokenMetadata
// only). Their own instructions; the same accounts, rules and request layout.
// ===========================================================================
const TOKEN22: Pubkey = anchor_spl::token_2022::ID;
const EXT_MINT_CLOSE_AUTHORITY: u16 = 3;
const EXT_IMMUTABLE_OWNER: u16 = 7;
const EXT_PERMANENT_DELEGATE: u16 = 12;
const EXT_TRANSFER_HOOK: u16 = 14;
const EXT_METADATA_POINTER: u16 = 18;
const EXT_TOKEN_METADATA: u16 = 19;
const ANCHOR_ACCOUNT_OWNED_BY_WRONG_PROGRAM: u32 = 3007;

fn ata22(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[owner.as_ref(), TOKEN22.as_ref(), mint.as_ref()], &ATA).0
}
fn tlv(out: &mut Vec<u8>, ty: u16, value: &[u8]) {
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
}
fn borsh_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}
/// Metadata kept in the mint: the pointer and the metadata itself, no
/// authority over either.
fn metadata_extensions(mint: &Pubkey) -> Vec<(u16, Vec<u8>)> {
    let mut pointer = vec![0u8; 32];
    pointer.extend_from_slice(mint.as_ref());
    let mut meta = vec![0u8; 32];
    meta.extend_from_slice(mint.as_ref());
    borsh_str(&mut meta, "Sample Coin");
    borsh_str(&mut meta, "SAMPLE");
    borsh_str(&mut meta, "https://ipfs.io/ipfs/bafkreisample");
    meta.extend_from_slice(&0u32.to_le_bytes());
    vec![(EXT_METADATA_POINTER, pointer), (EXT_TOKEN_METADATA, meta)]
}
/// A Token-2022 mint (base, then the account type and its TLV extensions).
fn new_mint22(
    svm: &mut LiteSVM,
    supply: u64,
    decimals: u8,
    auth: Option<Pubkey>,
    extensions: impl FnOnce(&Pubkey) -> Vec<(u16, Vec<u8>)>,
) -> Pubkey {
    let k = Pubkey::new_unique();
    let mut d = mint_bytes(supply, decimals, auth, None);
    let ext = extensions(&k);
    if !ext.is_empty() {
        d.resize(165, 0);
        d.push(1); // AccountType::Mint
        for (t, v) in &ext {
            tlv(&mut d, *t, v);
        }
    }
    put(svm, k, TOKEN22, d);
    k
}
/// A holder's Token-2022 account (as the ATA program makes it: ImmutableOwner).
fn new_token_account22(svm: &mut LiteSVM, mint: &Pubkey, owner: &Pubkey, amount: u64) -> Pubkey {
    let k = ata22(owner, mint);
    let mut d = token_bytes(mint, owner, amount);
    d.push(2); // AccountType::Account
    tlv(&mut d, EXT_IMMUTABLE_OWNER, &[]);
    put(svm, k, TOKEN22, d);
    k
}

fn register22_ix(admin: &Pubkey, mint: &Pubkey, limits: MintLimits) -> Instruction {
    ix_of(
        ix::RegisterMint2022 { limits },
        acc::RegisterMint2022 {
            admin: *admin,
            config: config_pda(),
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            vault: vault_pda(mint),
            token_program: TOKEN22,
            system_program: SYSTEM,
        },
    )
}
fn bridge22_ix(env: &Env, user: &Pubkey, user_token: &Pubkey, rid: u64, amount: u64, wp: [u8; 32]) -> Instruction {
    bridge22_ix_for(&env.fee_receiver, &env.mint, user, user_token, rid, amount, wp)
}
fn bridge22_ix_for(
    fee_receiver: &Pubkey,
    mint: &Pubkey,
    user: &Pubkey,
    user_token: &Pubkey,
    rid: u64,
    amount: u64,
    wp: [u8; 32],
) -> Instruction {
    ix_of(
        ix::BridgeToRune2022 { request_id: rid, amount, btc_witness_program: wp },
        acc::BridgeToRune2022 {
            user: *user,
            config: config_pda(),
            fee_receiver: *fee_receiver,
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            user_token: *user_token,
            vault: vault_pda(mint),
            withdrawal: withdrawal_pda_of(mint, user, rid),
            token_program: TOKEN22,
            system_program: SYSTEM,
        },
    )
}
fn refund22_ix(attester: &Pubkey, mint: &Pubkey, rid: u64, wallet: &Pubkey) -> Instruction {
    ix_of(
        ix::RefundWithdrawal2022 {},
        acc::RefundWithdrawal2022 {
            attester: *attester,
            config: config_pda(),
            withdrawal: withdrawal_pda(mint, rid),
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            vault: vault_pda(mint),
            user: *wallet,
            user_token: ata22(wallet, mint),
            token_program: TOKEN22,
            associated_token_program: ATA,
            system_program: SYSTEM,
        },
    )
}
fn release22_ix(attester: &Pubkey, mint: &Pubkey, txid: [u8; 32], vout: u32, amount: u64, recipient: &Pubkey) -> Instruction {
    ix_of(
        ix::ReleaseDeposit2022 { btc_txid: txid, vout, amount },
        acc::ReleaseDeposit2022 {
            attester: *attester,
            config: config_pda(),
            mint: *mint,
            mint_bridge: mint_bridge_pda(mint),
            vault: vault_pda(mint),
            receipt: receipt_pda(mint, &txid, vout),
            recipient: *recipient,
            recipient_token: ata22(recipient, mint),
            token_program: TOKEN22,
            associated_token_program: ATA,
            system_program: SYSTEM,
        },
    )
}

/// Fresh chain with one Token-2022 coin (metadata in the mint), registered
/// and enabled; the user holds USER_BAL of it.
fn enabled22() -> Env {
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
    let mint = new_mint22(&mut svm, ETCHY_SUPPLY, 6, None, metadata_extensions);
    let user_token = new_token_account22(&mut svm, &mint, &user.pubkey(), USER_BAL);
    let args = InitializeArgs {
        admin: admin.pubkey(),
        guardian: guardian.pubkey(),
        attester: attester.pubkey(),
        fee_receiver,
        withdrawal_fee_lamports: FEE,
    };
    send(&mut svm, &[initialize_ix(&admin.pubkey(), &admin.pubkey(), args)], &[&admin]).unwrap();
    let limits = MintLimits { min_withdrawal: MIN, daily_release_cap: CAP };
    send(&mut svm, &[register22_ix(&admin.pubkey(), &mint, limits)], &[&admin]).unwrap();
    send(&mut svm, &[enable_ix(&admin.pubkey(), &mint, 970_711, 200)], &[&admin]).unwrap();
    Env { svm, admin, guardian, attester, fee_receiver, user, mint, user_token }
}
fn lock22(env: &mut Env, rid: u64, amount: u64) {
    let user = env.user.insecure_clone();
    let i = bridge22_ix(env, &user.pubkey(), &env.user_token.clone(), rid, amount, wp(7));
    send(&mut env.svm, &[i], &[&user]).unwrap();
}

#[test]
fn token_2022_register_accepts_only_metadata_only_etchy_coins() {
    let mut env = enabled22();
    let admin = env.admin.insecure_clone();
    let limits = || MintLimits { min_withdrawal: MIN, daily_release_cap: CAP };
    // The coin's vault is a plain Token-2022 account of the bridge (no
    // extension), empty.
    let vault = env.svm.get_account(&vault_pda(&env.mint)).unwrap();
    assert_eq!((vault.owner, vault.data.len()), (TOKEN22, 165));
    assert_eq!(Pubkey::try_from(&vault.data[32..64]).unwrap(), mint_bridge_pda(&env.mint));
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 0);
    let b: MintBridge = read(&env.svm, &mint_bridge_pda(&env.mint));
    assert_eq!((b.mint, b.vault, b.enabled), (env.mint, vault_pda(&env.mint), true));

    // Anything that could tax, block or take the vault's tokens is refused.
    let someone = Pubkey::new_unique();
    let with = |ty: u16, value: Vec<u8>| {
        move |m: &Pubkey| {
            let mut e = metadata_extensions(m);
            e.push((ty, value));
            e
        }
    };
    let refused = [
        new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, with(EXT_PERMANENT_DELEGATE, someone.to_bytes().to_vec())),
        new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, with(EXT_MINT_CLOSE_AUTHORITY, someone.to_bytes().to_vec())),
    ];
    for m in refused {
        let e = send(&mut env.svm, &[register22_ix(&admin.pubkey(), &m, limits())], &[&admin]).unwrap_err();
        assert_eq!(e, bridge_err(BridgeError::UnsupportedTokenExtension));
        assert!(env.svm.get_account(&mint_bridge_pda(&m)).is_none());
    }
    // A transfer hook: refused too.
    let mut hook = someone.to_bytes().to_vec();
    hook.extend_from_slice(Pubkey::new_unique().as_ref());
    let hooked = new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, with(EXT_TRANSFER_HOOK, hook));
    assert!(send(&mut env.svm, &[register22_ix(&admin.pubkey(), &hooked, limits())], &[&admin]).is_err());
    assert!(env.svm.get_account(&mint_bridge_pda(&hooked)).is_none());

    // The same Etchy-coin shape as SPL coins.
    let bad_shape = [
        new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, Some(someone), metadata_extensions), // can still mint
        new_mint22(&mut env.svm, ETCHY_SUPPLY, 9, None, metadata_extensions),          // wrong decimals
        new_mint22(&mut env.svm, ETCHY_SUPPLY + 1, 6, None, metadata_extensions),      // over the premine
        new_mint22(&mut env.svm, 0, 6, None, metadata_extensions),                     // empty
    ];
    for m in bad_shape {
        let e = send(&mut env.svm, &[register22_ix(&admin.pubkey(), &m, limits())], &[&admin]).unwrap_err();
        assert_eq!(e, bridge_err(BridgeError::NotAnEtchyCoin));
    }
    // Admin only; once per coin; a plain Token-2022 mint (no extension) is fine.
    let good = new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, metadata_extensions);
    let stranger = env.user.insecure_clone();
    let e = send(&mut env.svm, &[register22_ix(&stranger.pubkey(), &good, limits())], &[&stranger]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    send(&mut env.svm, &[register22_ix(&admin.pubkey(), &good, limits())], &[&admin]).unwrap();
    let e = send(&mut env.svm, &[register22_ix(&admin.pubkey(), &good, limits())], &[&admin]).unwrap_err();
    assert_eq!(e, custom(SYSTEM_ACCOUNT_IN_USE));
    let plain = new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, |_| vec![]);
    send(&mut env.svm, &[register22_ix(&admin.pubkey(), &plain, limits())], &[&admin]).unwrap();

    // An SPL coin is never registered as a Token-2022 one, and the reverse.
    let spl = new_mint(&mut env.svm, ETCHY_SUPPLY, 6, None, None);
    assert!(send(&mut env.svm, &[register22_ix(&admin.pubkey(), &spl, limits())], &[&admin]).is_err());
    let t22 = new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, metadata_extensions);
    let e = send(&mut env.svm, &[register_ix(&admin.pubkey(), &t22, limits())], &[&admin]).unwrap_err();
    assert_eq!(e, custom(ANCHOR_ACCOUNT_OWNED_BY_WRONG_PROGRAM));
}

#[test]
fn token_2022_bridge_to_rune_records_the_same_request_the_cosigner_reads() {
    let mut env = enabled22();
    let fee_before = env.svm.get_account(&env.fee_receiver).unwrap().lamports;
    lock22(&mut env, 9, 40_000_000);
    assert_eq!(balance(&env.svm, &env.user_token), USER_BAL - 40_000_000);
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 40_000_000);
    assert_eq!(env.svm.get_account(&env.fee_receiver).unwrap().lamports, fee_before + FEE);
    let key = withdrawal_pda(&env.mint, 9);
    let w: WithdrawalRequest = read(&env.svm, &key);
    assert_eq!(
        (w.mint, w.user, w.user_token, w.amount, w.status, w.seq, w.request_id),
        (env.mint, default_user().pubkey(), env.user_token, 40_000_000, WithdrawalStatus::Pending, 0, 9)
    );
    // Byte for byte what the Internet Computer co-signer parses.
    let raw = env.svm.get_account(&key).unwrap().data;
    assert_eq!(raw.len(), 202);
    assert_eq!(&raw[8..40], env.mint.as_ref());
    assert_eq!(u64::from_le_bytes(raw[120..128].try_into().unwrap()), 40_000_000);
    assert_eq!(&raw[128..160], &wp(7));
    assert_eq!(raw[160], 0);
    let b = env.svm.get_account(&mint_bridge_pda(&env.mint)).unwrap().data;
    assert_eq!((&b[8..40], b[84], b[85]), (env.mint.as_ref(), 1, 0));
    assert_eq!(u64::from_le_bytes(b[72..80].try_into().unwrap()), 970_711);

    // Bad input: same rules as SPL coins.
    let user = env.user.insecure_clone();
    let ut = env.user_token;
    let i = bridge22_ix(&env, &user.pubkey(), &ut, 10, MIN - 1, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::AmountTooSmall));
    let i = bridge22_ix(&env, &user.pubkey(), &ut, 10, MIN, [0; 32]);
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::InvalidBtcAddress));
    let mut wrong_fee = bridge22_ix(&env, &user.pubkey(), &ut, 10, MIN, wp(7));
    wrong_fee.accounts[2].pubkey = Pubkey::new_unique();
    assert_eq!(send(&mut env.svm, &[wrong_fee], &[&user]).unwrap_err(), bridge_err(BridgeError::InvalidFeeReceiver));
    // Someone else's tokens cannot be locked.
    let thief = Keypair::new();
    env.svm.airdrop(&thief.pubkey(), 1_000_000_000).unwrap();
    let i = bridge22_ix(&env, &thief.pubkey(), &ut, 10, MIN, wp(7));
    assert!(send(&mut env.svm, &[i], &[&thief]).is_err());
    assert_eq!(balance(&env.svm, &ut), USER_BAL - 40_000_000);
    // Not before the Rune is final.
    let admin = env.admin.insecure_clone();
    let fresh = new_mint22(&mut env.svm, ETCHY_SUPPLY, 6, None, metadata_extensions);
    send(
        &mut env.svm,
        &[register22_ix(&admin.pubkey(), &fresh, MintLimits { min_withdrawal: MIN, daily_release_cap: CAP })],
        &[&admin],
    )
    .unwrap();
    let fresh_token = new_token_account22(&mut env.svm, &fresh, &user.pubkey(), USER_BAL);
    let i = bridge22_ix_for(&env.fee_receiver, &fresh, &user.pubkey(), &fresh_token, 1, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&user]).unwrap_err(), bridge_err(BridgeError::NotEnabled));
    assert_eq!(balance(&env.svm, &fresh_token), USER_BAL);
}

#[test]
fn token_2022_refund_and_release_follow_the_same_rules() {
    let mut env = enabled22();
    let attester = env.attester.insecure_clone();
    let user = default_user().pubkey();
    lock22(&mut env, 0, 40_000_000);
    // Refund: only to the requester's own wallet, only by the attester, once.
    let stranger = Pubkey::new_unique();
    let mut to_stranger = refund22_ix(&attester.pubkey(), &env.mint, 0, &user);
    to_stranger.accounts[6].pubkey = stranger;
    to_stranger.accounts[7].pubkey = ata22(&stranger, &env.mint);
    assert_eq!(
        send(&mut env.svm, &[to_stranger], &[&attester]).unwrap_err(),
        bridge_err(BridgeError::WrongRefundAccount)
    );
    let u = env.user.insecure_clone();
    let e = send(&mut env.svm, &[refund22_ix(&u.pubkey(), &env.mint, 0, &user)], &[&u]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    send(&mut env.svm, &[refund22_ix(&attester.pubkey(), &env.mint, 0, &user)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &env.user_token), USER_BAL);
    let e = send(&mut env.svm, &[refund22_ix(&attester.pubkey(), &env.mint, 0, &user)], &[&attester]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::NotPending));
    // ...even after the user closed their token account (re-created).
    lock22(&mut env, 1, USER_BAL);
    let ut = env.user_token;
    env.svm
        .set_account(ut, Account { lamports: 0, data: vec![], owner: SYSTEM, executable: false, rent_epoch: 0 })
        .unwrap();
    send(&mut env.svm, &[refund22_ix(&attester.pubkey(), &env.mint, 1, &user)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ut), USER_BAL);
    assert_eq!(env.svm.get_account(&ut).unwrap().owner, TOKEN22);
    assert_invariant(&env);

    // Release: once per Bitcoin outpoint, to the recipient's own account
    // (created if missing), under the daily cap. (Request 2's Runes were
    // paid on Bitcoin: its tokens stay in the vault, backing what returns.)
    lock22(&mut env, 2, 2 * CAP);
    send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 2, [7; 32])], &[&attester]).unwrap();
    let alice = Pubkey::new_unique();
    let txid = [0xab; 32];
    send(&mut env.svm, &[release22_ix(&attester.pubkey(), &env.mint, txid, 1, 3_000_000_000, &alice)], &[&attester])
        .unwrap();
    assert_eq!(balance(&env.svm, &ata22(&alice, &env.mint)), 3_000_000_000);
    let r: DepositReceipt = read(&env.svm, &receipt_pda(&env.mint, &txid, 1));
    assert_eq!((r.amount, r.recipient, r.vout, r.btc_txid), (3_000_000_000, alice, 1, txid));
    let e = send(&mut env.svm, &[release22_ix(&attester.pubkey(), &env.mint, txid, 1, 1, &alice)], &[&attester])
        .unwrap_err();
    assert_eq!(e, custom(SYSTEM_ACCOUNT_IN_USE));
    let e = send(&mut env.svm, &[release22_ix(&u.pubkey(), &env.mint, [1; 32], 0, 1, &alice)], &[&u]).unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::Unauthorized));
    let e = send(&mut env.svm, &[release22_ix(&attester.pubkey(), &env.mint, [0; 32], 0, 1, &alice)], &[&attester])
        .unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::InvalidBtcTxid));
    let e = send(&mut env.svm, &[release22_ix(&attester.pubkey(), &env.mint, [1; 32], 0, CAP, &alice)], &[&attester])
        .unwrap_err();
    assert_eq!(e, bridge_err(BridgeError::DailyCapExceeded));
    set_time(&mut env.svm, NOW + 86_400);
    send(&mut env.svm, &[release22_ix(&attester.pubkey(), &env.mint, [1; 32], 0, CAP, &alice)], &[&attester]).unwrap();
    assert_invariant(&env);

    // Pause: nothing locks or releases; pending users can still be refunded.
    lock22(&mut env, 3, 5 * MIN);
    let guardian = env.guardian.insecure_clone();
    send(&mut env.svm, &[pause_ix(&guardian.pubkey())], &[&guardian]).unwrap();
    let i = bridge22_ix(&env, &user, &ut, 4, MIN, wp(7));
    assert_eq!(send(&mut env.svm, &[i], &[&u]).unwrap_err(), bridge_err(BridgeError::Paused));
    let r = release22_ix(&attester.pubkey(), &env.mint, [3; 32], 0, MIN, &alice);
    assert_eq!(send(&mut env.svm, &[r], &[&attester]).unwrap_err(), bridge_err(BridgeError::Paused));
    let before = balance(&env.svm, &ut);
    send(&mut env.svm, &[refund22_ix(&attester.pubkey(), &env.mint, 3, &user)], &[&attester]).unwrap();
    assert_eq!(balance(&env.svm, &ut), before + 5 * MIN);
    assert_invariant(&env);
}

#[test]
fn token_2022_and_spl_instructions_never_mix() {
    // A Token-2022 coin cannot be moved by the SPL instructions...
    let mut env = enabled22();
    let (user, attester) = (env.user.insecure_clone(), env.attester.insecure_clone());
    let (ut, fr) = (env.user_token, env.fee_receiver);
    let i = bridge_ix(&env, &user.pubkey(), &ut, &fr, 0, MIN, wp(7));
    assert!(send(&mut env.svm, &[i], &[&user]).is_err());
    lock22(&mut env, 1, 10 * MIN);
    let alice = Pubkey::new_unique();
    let r = release_ix(&attester.pubkey(), &env.mint, [5; 32], 0, MIN, &alice);
    assert!(send(&mut env.svm, &[r], &[&attester]).is_err());
    let r = refund_ix(&attester.pubkey(), &env.mint, 1, &ut);
    assert!(send(&mut env.svm, &[r], &[&attester]).is_err());
    assert_eq!(balance(&env.svm, &vault_pda(&env.mint)), 10 * MIN);
    assert_invariant(&env);

    // ...and an SPL coin cannot be moved by the Token-2022 ones.
    let mut spl = enabled();
    lock(&mut spl, 0, 10 * MIN);
    let (u2, a2) = (spl.user.insecure_clone(), spl.attester.insecure_clone());
    let ut2 = spl.user_token;
    let i = bridge22_ix(&spl, &u2.pubkey(), &ut2, 1, MIN, wp(7));
    assert!(send(&mut spl.svm, &[i], &[&u2]).is_err());
    let r = release22_ix(&a2.pubkey(), &spl.mint, [6; 32], 0, MIN, &alice);
    assert!(send(&mut spl.svm, &[r], &[&a2]).is_err());
    let r = refund22_ix(&a2.pubkey(), &spl.mint, 0, &u2.pubkey());
    assert!(send(&mut spl.svm, &[r], &[&a2]).is_err());
    assert_eq!(balance(&spl.svm, &vault_pda(&spl.mint)), 10 * MIN);
    assert_invariant(&spl);
}

#[test]
fn token_2022_round_trip_keeps_rune_custody_equal_to_circulating_supply() {
    let mut env = enabled22();
    let mut custody: u64 = ETCHY_SUPPLY;
    let circulating = |env: &Env| ETCHY_SUPPLY - balance(&env.svm, &vault_pda(&env.mint));
    let attester = env.attester.insecure_clone();
    lock22(&mut env, 0, 300_000_000);
    send(&mut env.svm, &[sent_ix(&attester.pubkey(), &env.mint, 0, [8; 32])], &[&attester]).unwrap();
    custody -= 300_000_000;
    assert_eq!(custody, circulating(&env));
    lock22(&mut env, 1, 50_000_000);
    send(&mut env.svm, &[refund22_ix(&attester.pubkey(), &env.mint, 1, &default_user().pubkey())], &[&attester])
        .unwrap();
    assert_eq!(custody, circulating(&env));
    custody += 120_000_000;
    let bob = Pubkey::new_unique();
    send(&mut env.svm, &[release22_ix(&attester.pubkey(), &env.mint, [2; 32], 0, 120_000_000, &bob)], &[&attester])
        .unwrap();
    assert_eq!(custody, circulating(&env));
    assert_eq!(balance(&env.svm, &ata22(&bob, &env.mint)), 120_000_000);
    assert_invariant(&env);
}
