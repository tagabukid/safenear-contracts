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
const MAX_TAX_BPS: u16 = 1_000;
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
        U128(self.creation_fee)
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

    /* ------------------------------- launch ------------------------------- */

    /// Launch a token. Attach at least `get_creation_fee()` and 300 Tgas.
    #[payable]
    pub fn create_token(&mut self, name: String, symbol: String, tax_bps: u16) -> Promise {
        let creator = env::predecessor_account_id();
        let deposit = env::attached_deposit().as_yoctonear();
        require!(
            deposit >= self.creation_fee,
            format!("Attach at least {} yoctoNEAR", self.creation_fee)
        );
        let name = name.trim().to_string();
        require!(!name.is_empty() && name.len() <= 32, "Name must be 1 to 32 characters");
        let symbol = symbol.trim().to_uppercase();
        require!(
            (2..=10).contains(&symbol.len()) && symbol.chars().all(|c| c.is_ascii_alphanumeric()),
            "Ticker must be 2 to 10 letters or numbers"
        );
        require!(tax_bps <= MAX_TAX_BPS, "Tax above 10% is not allowed");

        let token_id: AccountId = format!("{}.{}", symbol.to_lowercase(), env::current_account_id())
            .parse()
            .expect("Invalid token account");
        require!(!self.ids.contains(&token_id), "That ticker is already taken");
        self.ids.insert(token_id.clone()); // reserved; released if creation fails

        let args = json!({
            "name": name,
            "symbol": symbol,
            "icon": null,
            "creator": creator,
            "tax_bps": tax_bps,
            "graduation_threshold": U128(self.graduation_threshold),
            "virtual_near": U128(self.virtual_near),
            "ref_contract": self.ref_contract,
            "wrap_contract": self.wrap_contract,
        })
        .to_string()
        .into_bytes();

        // No add_full_access_key / add_access_key: the token account is keyless and immutable.
        Promise::new(token_id.clone())
            .create_account()
            .transfer(NearToken::from_yoctonear(self.token_account_balance))
            .deploy_contract(TOKEN_WASM.to_vec())
            .function_call("new".to_string(), args, NearToken::from_yoctonear(0), Gas::from_tgas(30))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(Gas::from_tgas(20))
                    .on_token_created(token_id, name, symbol, creator, U128(deposit)),
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
            let extra = deposit.0 - self.creation_fee;
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
        if !self.points_open {
            return;
        }
        let e = self.entry(&account_id);
        match kind.as_str() {
            "buy" | "sell" => {
                let rate = if kind == "buy" { PTS_PER_NEAR_BUY } else { PTS_PER_NEAR_SELL };
                let earned = (near_amount.0 * rate / ONE_NEAR) as u32;
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

    /// Withdraw platform fees, keeping enough balance to cover the factory's own storage.
    pub fn withdraw_fees(&mut self, amount: U128) -> Promise {
        self.assert_owner();
        let locked = env::storage_usage() as u128 * env::storage_byte_cost().as_yoctonear();
        let free = env::account_balance().as_yoctonear().saturating_sub(locked);
        require!(amount.0 <= free, "Not enough free balance");
        Promise::new(self.owner.clone()).transfer(NearToken::from_yoctonear(amount.0))
    }
}
