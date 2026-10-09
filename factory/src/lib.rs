//! SafeNear factory: creates one token contract per launch at `<ticker>.<factory>`.
//!
//! The new account gets NO access keys, so nobody (not the creator, not the factory owner)
//! can ever change its code or move its funds outside the contract's own rules.

use near_sdk::json_types::{U128, U64};
use near_sdk::serde_json::json;
use near_sdk::store::{IterableMap, LookupSet, Vector};
use near_sdk::{
    env, log, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise,
    PromiseResult,
};

/// Built by scripts/build.sh before the factory is compiled.
const TOKEN_WASM: &[u8] = include_bytes!("../../res/safenear_token.wasm");
const MAX_TAX_BPS: u16 = 400; // 4% a side
/// Global-contract launch settings live under their own storage key, so adding them
/// doesn't change the factory's existing state layout (no migration needed).
const GLOBAL_KEY: &[u8] = b"__safenear_global";
/// SafeNear platform fee settings, also under their own key (no state migration).
const PLATFORM_KEY: &[u8] = b"__safenear_platform";
const DEFAULT_PLATFORM_FEE_BPS: u16 = 50; // 0.5% per trade
const MAX_PLATFORM_FEE_BPS: u16 = 100; // 1%
const MIN_DEV_BUY: u128 = 10_000_000_000_000_000_000_000; // 0.01 NEAR
const MAX_ICON_LEN: usize = 12_000;
const ONE_NEAR: u128 = 1_000_000_000_000_000_000_000_000;
const DAY_MS: u64 = 86_400_000;

/* ---- testnet points rules (airdrop campaign) ---- */
const PTS_CREATE: u64 = 50; // per token launched
const MAX_CREATES_PER_DAY: u8 = 3; // only the first 3 launches per day earn points
const PTS_PER_NEAR_BUY: u128 = 10; // 1 point per 0.1 NEAR bought
const PTS_PER_NEAR_SELL: u128 = 2;
const MAX_TRADE_PTS_PER_DAY: u32 = 300; // daily cap so wash trading can't run away
const PTS_GRADUATED: u64 = 500; // to the creator when their token fills its curve
const PTS_MIGRATE_STEP: u64 = 10; // to whoever pushes a graduation step

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Tokens,
    Ids,
    Live,
    Points,
}

#[near(serializers = [borsh])]
#[derive(Clone, Default)]
pub struct PointsEntry {
    pub points: u64,
    pub day: u64,
    pub day_trade_pts: u32,
    pub day_creates: u8,
    pub mainnet_account: Option<String>,
}

#[near(serializers = [json])]
pub struct PointsView {
    pub account_id: AccountId,
    pub points: U64,
    pub mainnet_account: Option<String>,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct TokenInfo {
    pub token_id: AccountId,
    pub name: String,
    pub symbol: String,
    pub creator: AccountId,
    pub created_at_ms: U64,
}

#[near(serializers = [borsh])]
#[derive(Clone)]
pub struct GlobalMode {
    pub enabled: bool,
    pub code_hash: [u8; 32],
    pub creation_fee: u128,
    pub token_account_balance: u128,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct PlatformFee {
    /// fee on every curve buy and sell, in basis points (50 = 0.5%)
    pub fee_bps: u16,
    /// where the fee goes; the factory itself by default (owner withdraws with withdraw_fees)
    pub account: AccountId,
}

#[near(serializers = [json])]
pub struct LaunchModeView {
    /// true = tokens use the shared global token code (cheap launches)
    pub global: bool,
    pub code_hash_hex: Option<String>,
    pub creation_fee: U128,
    pub token_account_balance: U128,
    pub token_code_bytes: u64,
    /// One-time NEAR burned to publish the token code globally (10 NEAR per 100 KB)
    pub publish_cost: U128,
}

#[near(serializers = [json])]
pub struct Config {
    pub owner: AccountId,
    pub creation_fee: U128,
    pub token_account_balance: U128,
    pub graduation_threshold: U128,
    pub virtual_near: U128,
    pub ref_contract: AccountId,
    pub wrap_contract: AccountId,
    pub token_code_bytes: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Factory {
    owner: AccountId,
    tokens: Vector<TokenInfo>,
    ids: LookupSet<AccountId>,
    /// Token contracts that launched successfully (only these can report points).
    live: LookupSet<AccountId>,
    points: IterableMap<AccountId, PointsEntry>,
    points_open: bool,
    /// Total NEAR the creator attaches.
    creation_fee: u128,
    /// Part of the fee sent to the new token account (pays its contract storage + graduation
    /// storage deposits). The rest stays with the factory as the platform fee.
    token_account_balance: u128,
    graduation_threshold: u128,
    virtual_near: u128,
    ref_contract: AccountId,
    wrap_contract: AccountId,
}

#[near]
impl Factory {
    #[init]
    pub fn new(
        owner: AccountId,
        creation_fee: U128,
        token_account_balance: U128,
        graduation_threshold: U128,
        virtual_near: U128,
        ref_contract: AccountId,
        wrap_contract: AccountId,
    ) -> Self {
        require!(
            token_account_balance.0 <= creation_fee.0,
            "token_account_balance must be <= creation_fee"
        );
        Self {
            owner,
            tokens: Vector::new(StorageKey::Tokens),
            ids: LookupSet::new(StorageKey::Ids),
            live: LookupSet::new(StorageKey::Live),
            points: IterableMap::new(StorageKey::Points),
            points_open: true,
            creation_fee: creation_fee.0,
            token_account_balance: token_account_balance.0,
            graduation_threshold: graduation_threshold.0,
            virtual_near: virtual_near.0,
            ref_contract,
            wrap_contract,
        }
    }

    /* -------------------------------- views -------------------------------- */

    pub fn get_tokens_count(&self) -> u64 {
        self.tokens.len() as u64
    }

    pub fn get_tokens(&self, from_index: Option<u64>, limit: Option<u64>) -> Vec<TokenInfo> {
        let from = from_index.unwrap_or(0) as u32;
        let limit = limit.unwrap_or(40).min(100) as u32;
        (from..self.tokens.len().min(from.saturating_add(limit)))
            .filter_map(|i| self.tokens.get(i).cloned())
            .collect()
    }

    pub fn get_creation_fee(&self) -> U128 {
        U128(self.launch_terms().0)
    }

    pub fn get_config(&self) -> Config {
        Config {
            owner: self.owner.clone(),
            creation_fee: U128(self.creation_fee),
            token_account_balance: U128(self.token_account_balance),
            graduation_threshold: U128(self.graduation_threshold),
            virtual_near: U128(self.virtual_near),
            ref_contract: self.ref_contract.clone(),
            wrap_contract: self.wrap_contract.clone(),
            token_code_bytes: TOKEN_WASM.len() as u64,
        }
    }

    fn platform_fee(&self) -> PlatformFee {
        env::storage_read(PLATFORM_KEY)
            .and_then(|b| near_sdk::borsh::from_slice::<PlatformFee>(&b).ok())
            .unwrap_or(PlatformFee { fee_bps: DEFAULT_PLATFORM_FEE_BPS, account: env::current_account_id() })
    }

    pub fn get_platform_fee(&self) -> PlatformFee {
        self.platform_fee()
    }

    fn global_mode(&self) -> Option<GlobalMode> {
        env::storage_read(GLOBAL_KEY)
            .and_then(|b| near_sdk::borsh::from_slice::<GlobalMode>(&b).ok())
            .filter(|g| g.enabled)
    }

    /// (creation fee, NEAR sent to the new token account, global code hash if enabled)
    fn launch_terms(&self) -> (u128, u128, Option<[u8; 32]>) {
        match self.global_mode() {
            Some(g) => (g.creation_fee, g.token_account_balance, Some(g.code_hash)),
            None => (self.creation_fee, self.token_account_balance, None),
        }
    }

    pub fn get_launch_mode(&self) -> LaunchModeView {
        let (fee, bal, hash) = self.launch_terms();
        LaunchModeView {
            global: hash.is_some(),
            code_hash_hex: hash.map(|h| h.iter().map(|b| format!("{:02x}", b)).collect()),
            creation_fee: U128(fee),
            token_account_balance: U128(bal),
            token_code_bytes: TOKEN_WASM.len() as u64,
            publish_cost: U128(TOKEN_WASM.len() as u128 * 10 * env::storage_byte_cost().as_yoctonear()),
        }
    }

    /* ------------------------------- launch ------------------------------- */

    /// Launch a token. Attach at least `get_creation_fee()` and 300 Tgas.
    /// Any NEAR attached above the creation fee becomes the creator's buy at launch
    /// (no tax, capped at 5% of supply by the token contract, unused part refunded).
    #[payable]
    pub fn create_token(
        &mut self,
        name: String,
        symbol: String,
        buy_tax_bps: u16,
        sell_tax_bps: u16,
        fee_recipient: Option<AccountId>,
        burn_bps: Option<u16>,
        icon: Option<String>,
        description: Option<String>,
        website: Option<String>,
        twitter: Option<String>,
        telegram: Option<String>,
    ) -> Promise {
        let creator = env::predecessor_account_id();
        let deposit = env::attached_deposit().as_yoctonear();
        let (fee, account_balance, global_hash) = self.launch_terms();
        require!(deposit >= fee, format!("Attach at least {} yoctoNEAR", fee));
        let name = name.trim().to_string();
        require!(!name.is_empty() && name.len() <= 32, "Name must be 1 to 32 characters");
        let symbol = symbol.trim().to_uppercase();
        require!(
            (2..=10).contains(&symbol.len()) && symbol.chars().all(|c| c.is_ascii_alphanumeric()),
            "Ticker must be 2 to 10 letters or numbers"
        );
        require!(
            buy_tax_bps <= MAX_TAX_BPS && sell_tax_bps <= MAX_TAX_BPS,
            "Tax above 4% a side is not allowed"
        );
        require!(burn_bps.unwrap_or(0) <= 10_000, "burn_bps must be between 0 and 10000");
        if let Some(i) = &icon {
            require!(
                i.len() <= MAX_ICON_LEN && (i.starts_with("data:image/") || i.starts_with("https://")),
                "Image must be a small data:image or https:// link (max 12 KB)"
            );
        }
        for (v, what) in [(&website, "Website"), (&twitter, "X link"), (&telegram, "Telegram link")] {
            if let Some(l) = v {
                require!(l.len() <= 200 && l.starts_with("https://"), format!("{} must be an https:// link", what));
            }
        }
        if let Some(d) = &description {
            require!(d.len() <= 280, "Description is too long (max 280 characters)");
        }
        let extra = deposit - fee;
        let dev_buy = if extra >= MIN_DEV_BUY { extra } else { 0 };

        let token_id: AccountId = format!("{}.{}", symbol.to_lowercase(), env::current_account_id())
            .parse()
            .expect("Invalid token account");
        require!(!self.ids.contains(&token_id), "That ticker is already taken");
        self.ids.insert(token_id.clone()); // reserved; released if creation fails

        let args = json!({
            "name": name,
            "symbol": symbol,
            "icon": icon,
            "description": description,
            "website": website,
            "twitter": twitter,
            "telegram": telegram,
            "initial_buy": U128(dev_buy),
            "creator": creator,
            "buy_tax_bps": buy_tax_bps,
            "sell_tax_bps": sell_tax_bps,
            "fee_recipient": fee_recipient.unwrap_or_else(|| creator.clone()),
            "burn_bps": burn_bps.unwrap_or(0),
            "platform_fee_bps": self.platform_fee().fee_bps,
            "platform_account": self.platform_fee().account,
            "graduation_threshold": U128(self.graduation_threshold),
            "virtual_near": U128(self.virtual_near),
            "ref_contract": self.ref_contract,
            "wrap_contract": self.wrap_contract,
        })
        .to_string()
        .into_bytes();

        // No add_full_access_key / add_access_key: the token account is keyless and immutable.
        let p = Promise::new(token_id.clone())
            .create_account()
            .transfer(NearToken::from_yoctonear(account_balance + dev_buy));
        // Global mode: point the account at the shared token code (by hash, so it can never change).
        let p = match global_hash {
            Some(h) => p.use_global_contract(h),
            None => p.deploy_contract(TOKEN_WASM.to_vec()),
        };
        p.function_call("new".to_string(), args, NearToken::from_yoctonear(0), Gas::from_tgas(60))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(Gas::from_tgas(20))
                    .on_token_created(token_id, name, symbol, creator, U128(deposit), U128(dev_buy), U128(fee)),
            )
    }

    #[private]
    pub fn on_token_created(
        &mut self,
        token_id: AccountId,
        name: String,
        symbol: String,
        creator: AccountId,
        deposit: U128,
        dev_buy: U128,
        fee: U128,
    ) -> bool {
        let ok = matches!(env::promise_result(0), PromiseResult::Successful(_));
        let creator_for_points = creator.clone();
        if ok {
            self.tokens.push(TokenInfo {
                token_id: token_id.clone(),
                name,
                symbol,
                creator: creator.clone(),
                created_at_ms: U64(env::block_timestamp_ms()),
            });
            if dev_buy.0 > 0 {
                self.award_trade(&creator_for_points, "buy", dev_buy.0);
            }
            // Leftover below the 0.01 NEAR dev-buy minimum goes back to the creator.
            let extra = deposit.0 - fee.0 - dev_buy.0;
            if extra > 0 {
                Promise::new(creator).transfer(NearToken::from_yoctonear(extra));
            }
            self.live.insert(token_id.clone());
            self.award_create(&creator_for_points);
            log!("LAUNCHED {}", token_id);
        } else {
            // The failed batch already returned the transferred NEAR to the factory.
            self.ids.remove(&token_id);
            Promise::new(creator).transfer(NearToken::from_yoctonear(deposit.0));
            log!("Launch of {} failed. Full refund sent.", token_id);
        }
        ok
    }

    /* ------------------------------- points ------------------------------- */

    fn entry(&mut self, account_id: &AccountId) -> &mut PointsEntry {
        let today = env::block_timestamp_ms() / DAY_MS;
        if !self.points.contains_key(account_id) {
            self.points.insert(account_id.clone(), PointsEntry::default());
        }
        let e = self.points.get_mut(account_id).unwrap();
        if e.day != today {
            e.day = today;
            e.day_trade_pts = 0;
            e.day_creates = 0;
        }
        e
    }

    fn award_create(&mut self, creator: &AccountId) {
        if !self.points_open {
            return;
        }
        let e = self.entry(creator);
        if e.day_creates < MAX_CREATES_PER_DAY {
            e.day_creates += 1;
            e.points += PTS_CREATE;
        }
    }

    /// Called by SafeNear token contracts only. kind: "buy" | "sell" | "graduated" | "migrate".
    /// Never panics on bad input so it can't break a trade.
    pub fn record_event(&mut self, account_id: AccountId, kind: String, near_amount: U128) {
        require!(
            self.live.contains(&env::predecessor_account_id()),
            "Only SafeNear tokens can report points"
        );
        self.award_trade(&account_id, &kind, near_amount.0);
    }

    fn award_trade(&mut self, account_id: &AccountId, kind: &str, near_amount: u128) {
        if !self.points_open {
            return;
        }
        let e = self.entry(account_id);
        match kind {
            "buy" | "sell" => {
                let rate = if kind == "buy" { PTS_PER_NEAR_BUY } else { PTS_PER_NEAR_SELL };
                let earned = (near_amount * rate / ONE_NEAR) as u32;
                let room = MAX_TRADE_PTS_PER_DAY.saturating_sub(e.day_trade_pts);
                let add = earned.min(room);
                e.day_trade_pts += add;
                e.points += add as u64;
            }
            "graduated" => e.points += PTS_GRADUATED,
            "migrate" => e.points += PTS_MIGRATE_STEP,
            _ => {}
        }
    }

    /// Link the mainnet wallet that should receive the airdrop.
    pub fn link_mainnet_account(&mut self, mainnet_account_id: String) {
        let parsed: Result<AccountId, _> = mainnet_account_id.parse();
        require!(parsed.is_ok(), "That is not a valid NEAR account");
        require!(
            !mainnet_account_id.ends_with(".testnet"),
            "Use your mainnet account (.near or a 64-character address), not .testnet"
        );
        let who = env::predecessor_account_id();
        self.entry(&who).mainnet_account = Some(mainnet_account_id);
    }

    pub fn get_points(&self, account_id: AccountId) -> Option<PointsView> {
        self.points.get(&account_id).map(|e| PointsView {
            account_id: account_id.clone(),
            points: U64(e.points),
            mainnet_account: e.mainnet_account.clone(),
        })
    }

    pub fn get_points_count(&self) -> u64 {
        self.points.len() as u64
    }

    /// Unsorted page; the frontend and export script sort by points.
    pub fn get_points_page(&self, from_index: Option<u64>, limit: Option<u64>) -> Vec<PointsView> {
        self.points
            .iter()
            .skip(from_index.unwrap_or(0) as usize)
            .take(limit.unwrap_or(100).min(200) as usize)
            .map(|(a, e)| PointsView {
                account_id: a.clone(),
                points: U64(e.points),
                mainnet_account: e.mainnet_account.clone(),
            })
            .collect()
    }

    pub fn is_points_open(&self) -> bool {
        self.points_open
    }

    /// Owner closes the campaign at snapshot time.
    pub fn set_points_open(&mut self, open: bool) {
        self.assert_owner();
        self.points_open = open;
    }

    /* -------------------------------- owner -------------------------------- */

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Only the owner can do this");
    }

    /// Changes apply to future launches only. Existing tokens are immutable.
    pub fn set_config(
        &mut self,
        creation_fee: Option<U128>,
        token_account_balance: Option<U128>,
        graduation_threshold: Option<U128>,
        virtual_near: Option<U128>,
    ) {
        self.assert_owner();
        if let Some(v) = creation_fee {
            self.creation_fee = v.0;
        }
        if let Some(v) = token_account_balance {
            self.token_account_balance = v.0;
        }
        if let Some(v) = graduation_threshold {
            self.graduation_threshold = v.0;
        }
        if let Some(v) = virtual_near {
            self.virtual_near = v.0;
        }
        require!(
            self.token_account_balance <= self.creation_fee,
            "token_account_balance must be <= creation_fee"
        );
    }

    /// Publish the token code once as a global contract (by hash) and switch launches to it.
    /// Burns about 10 NEAR per 100 KB of token code from the factory balance (see get_launch_mode).
    pub fn publish_token_code(&mut self, creation_fee: U128, token_account_balance: U128) -> Promise {
        self.assert_owner();
        require!(token_account_balance.0 <= creation_fee.0, "token_account_balance must be <= creation_fee");
        let hash = env::sha256_array(TOKEN_WASM);
        Promise::new(env::current_account_id())
            .deploy_global_contract(TOKEN_WASM.to_vec())
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(Gas::from_tgas(10))
                    .on_code_published(hash.to_vec(), creation_fee, token_account_balance),
            )
    }

    #[private]
    pub fn on_code_published(&mut self, hash: Vec<u8>, creation_fee: U128, token_account_balance: U128) -> bool {
        let ok = matches!(env::promise_result(0), PromiseResult::Successful(_));
        if ok {
            let mut code_hash = [0u8; 32];
            code_hash.copy_from_slice(&hash);
            let g = GlobalMode { enabled: true, code_hash, creation_fee: creation_fee.0, token_account_balance: token_account_balance.0 };
            env::storage_write(GLOBAL_KEY, &near_sdk::borsh::to_vec(&g).unwrap());
            log!("GLOBAL TOKEN CODE PUBLISHED. Launches now cost {} yoctoNEAR.", creation_fee.0);
        } else {
            log!("Publishing the global token code failed. Launches keep the old (full deploy) mode.");
        }
        ok
    }

    /// Adjust the cheap-launch fees, or turn global mode off (falls back to full deploys).
    pub fn set_global_config(&mut self, enabled: bool, creation_fee: Option<U128>, token_account_balance: Option<U128>) {
        self.assert_owner();
        let mut g: GlobalMode = env::storage_read(GLOBAL_KEY)
            .and_then(|b| near_sdk::borsh::from_slice(&b).ok())
            .expect("Publish the token code first");
        g.enabled = enabled;
        if let Some(v) = creation_fee { g.creation_fee = v.0; }
        if let Some(v) = token_account_balance { g.token_account_balance = v.0; }
        require!(g.token_account_balance <= g.creation_fee, "token_account_balance must be <= creation_fee");
        env::storage_write(GLOBAL_KEY, &near_sdk::borsh::to_vec(&g).unwrap());
    }

    /// Change the platform fee for FUTURE launches (existing tokens keep theirs). Max 1%.
    pub fn set_platform_fee(&mut self, fee_bps: u16, account: Option<AccountId>) {
        self.assert_owner();
        require!(fee_bps <= MAX_PLATFORM_FEE_BPS, "Platform fee above 1% is not allowed");
        let account = account.unwrap_or_else(|| self.platform_fee().account);
        env::storage_write(PLATFORM_KEY, &near_sdk::borsh::to_vec(&PlatformFee { fee_bps, account }).unwrap());
    }

    /// Withdraw platform fees, keeping enough balance to cover the factory's own storage.
    pub fn withdraw_fees(&mut self, amount: U128) -> Promise {
        self.assert_owner();
        let locked = env::storage_usage() as u128 * env::storage_byte_cost().as_yoctonear();
        let free = env::account_balance().as_yoctonear().saturating_sub(locked);
        require!(amount.0 <= free, "Not enough free balance");
        Promise::new(self.owner.clone()).transfer(NearToken::from_yoctonear(amount.0))
    }
}
