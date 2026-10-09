//! SafeNear token: NEP-141 fungible token with a built-in NEAR bonding curve.
//!
//! Safety properties (enforced by code, not promises):
//! - Fixed supply: 1,000,000,000 tokens are minted once in `new`. There is no mint function.
//! - No admin: the creator has no special powers. The factory deploys this contract to an
//!   account with no access keys, so the code can never be changed or upgraded.
//! - Tax cap: buy and sell tax are each capped at 4% (MAX_TAX_BPS) and fixed at launch.
//!   Tax is split between the fee wallet and a "burn" that stays in the curve, raising the
//!   price for every holder.
//! - Locked liquidity: on graduation, all raised NEAR + the LP token allocation go into a
//!   Ref Finance pool. The LP shares stay in this contract's Ref account and there is no
//!   method to withdraw them, so the liquidity is locked forever.

use near_contract_standards::fungible_token::core::FungibleTokenCore;
use near_contract_standards::fungible_token::events::FtMint;
use near_contract_standards::fungible_token::metadata::{
    FungibleTokenMetadata, FungibleTokenMetadataProvider, FT_METADATA_SPEC,
};
use near_contract_standards::fungible_token::resolver::FungibleTokenResolver;
use near_contract_standards::fungible_token::FungibleToken;
use near_contract_standards::storage_management::{
    StorageBalance, StorageBalanceBounds, StorageManagement,
};
use near_sdk::json_types::{U128, U64};
use near_sdk::serde_json;
use near_sdk::{
    assert_one_yocto, env, ext_contract, log, near, require, AccountId, BorshStorageKey, Gas,
    NearToken, PanicOnDefault, Promise, PromiseOrValue, PromiseResult,
};

uint::construct_uint! {
    pub struct U256(4);
}

const DECIMALS: u8 = 18;
const ONE_TOKEN: u128 = 1_000_000_000_000_000_000;
const TOTAL_SUPPLY: u128 = 1_000_000_000 * ONE_TOKEN;
/// 80% is sold on the bonding curve.
const CURVE_SUPPLY: u128 = 800_000_000 * ONE_TOKEN;
/// 20% is paired with the raised NEAR in the Ref Finance pool at graduation.
const LP_SUPPLY: u128 = TOTAL_SUPPLY - CURVE_SUPPLY;
const MAX_TAX_BPS: u16 = 400; // 4% a side
const BPS: u128 = 10_000;
const MIN_BUY: u128 = 10_000_000_000_000_000_000_000; // 0.01 NEAR
const MAX_TRADES: usize = 30;
/// Ref Finance pool fee: 0.3%
const REF_POOL_FEE: u32 = 30;
/// The final graduation step index; migration_step == DONE means LP is in place and locked.
const DONE: u8 = 7;
/// Creator's buy at launch is capped at 5% of supply.
const MAX_DEV_TOKENS: u128 = 50_000_000 * ONE_TOKEN;
const MAX_ICON_LEN: usize = 12_000;
const MAX_LINK_LEN: usize = 200;
const MAX_DESC_LEN: usize = 280;

fn check_link(v: &Option<String>, what: &str) {
    if let Some(l) = v {
        require!(
            l.len() <= MAX_LINK_LEN && l.starts_with("https://"),
            format!("{} must be an https:// link up to 200 characters", what)
        );
    }
}

fn tgas(n: u64) -> Gas {
    Gas::from_tgas(n)
}

fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    (U256::from(a) * U256::from(b) / U256::from(c)).as_u128()
}

fn mul_div_ceil(a: u128, b: u128, c: u128) -> u128 {
    let n = U256::from(a) * U256::from(b);
    let c = U256::from(c);
    ((n + c - U256::from(1u8)) / c).as_u128()
}

#[ext_contract(ext_ref)]
#[allow(dead_code)]
trait RefExchange {
    fn storage_deposit(&mut self, account_id: Option<AccountId>, registration_only: Option<bool>);
    fn register_tokens(&mut self, token_ids: Vec<AccountId>);
    fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> U128;
    fn add_simple_pool(&mut self, tokens: Vec<AccountId>, fee: u32) -> u64;
    fn add_liquidity(&mut self, pool_id: u64, amounts: Vec<U128>, min_amounts: Option<Vec<U128>>);
}

#[ext_contract(ext_wrap)]
#[allow(dead_code)]
trait WrapNear {
    fn storage_deposit(&mut self, account_id: Option<AccountId>, registration_only: Option<bool>);
    fn near_deposit(&mut self);
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> U128;
}

#[ext_contract(ext_factory)]
#[allow(dead_code)]
trait SafeNearFactory {
    fn record_event(&mut self, account_id: AccountId, kind: String, near_amount: U128);
}

#[near(serializers = [borsh])]
#[derive(BorshStorageKey)]
enum StorageKey {
    Token,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Trade {
    pub side: String,
    pub account_id: AccountId,
    pub near_amount: U128,
    pub token_amount: U128,
    pub timestamp_ms: U64,
}

#[near(serializers = [json])]
pub struct InfoView {
    pub icon: Option<String>,
    pub description: Option<String>,
    pub website: Option<String>,
    pub twitter: Option<String>,
    pub telegram: Option<String>,
    pub dev_buy_tokens: U128,
}

#[near(serializers = [json])]
pub struct CurveView {
    pub near_reserve: U128,
    pub graduation_threshold: U128,
    pub graduated: bool,
    /// the higher of buy/sell tax (kept for older frontends)
    pub tax_bps: u16,
    pub buy_tax_bps: u16,
    pub sell_tax_bps: u16,
    /// share of the tax kept in the curve (0..10000); the rest goes to fee_recipient
    pub burn_bps: u16,
    pub fee_recipient: AccountId,
    pub creator: AccountId,
    /// yoctoNEAR per 1 whole token
    pub price: U128,
    pub pool_id: Option<u64>,
    pub curve_tokens_left: U128,
    pub migration_step: u8,
    pub lp_locked: bool,
    pub fixed_supply: bool,
    pub max_tax_bps: u16,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    token: FungibleToken,
    name: String,
    symbol: String,
    icon: Option<String>,
    description: Option<String>,
    website: Option<String>,
    twitter: Option<String>,
    telegram: Option<String>,
    dev_buy_tokens: u128,
    creator: AccountId,
    factory: AccountId,
    buy_tax_bps: u16,
    sell_tax_bps: u16,
    burn_bps: u16,
    fee_recipient: AccountId,
    virtual_near: u128,
    virtual_tokens: u128,
    near_reserve: u128,
    curve_tokens_left: u128,
    graduation_threshold: u128,
    graduated: bool,
    ref_contract: AccountId,
    wrap_contract: AccountId,
    lp_near: u128,
    lp_tokens: u128,
    migration_step: u8,
    migration_busy: bool,
    migrate_caller: Option<AccountId>,
    pool_id: Option<u64>,
    trades: Vec<Trade>,
}

#[near]
impl Contract {
    /// Called once by the factory in the same batch that deploys this contract.
    #[init]
    pub fn new(
        name: String,
        symbol: String,
        icon: Option<String>,
        creator: AccountId,
        buy_tax_bps: u16,
        sell_tax_bps: u16,
        graduation_threshold: U128,
        virtual_near: U128,
        ref_contract: AccountId,
        wrap_contract: AccountId,
        description: Option<String>,
        website: Option<String>,
        twitter: Option<String>,
        telegram: Option<String>,
        initial_buy: Option<U128>,
        fee_recipient: Option<AccountId>,
        burn_bps: Option<u16>,
    ) -> Self {
        require!(
            buy_tax_bps <= MAX_TAX_BPS && sell_tax_bps <= MAX_TAX_BPS,
            "Tax above 4% a side is not allowed"
        );
        let burn_bps = burn_bps.unwrap_or(0);
        require!(burn_bps as u128 <= BPS, "burn_bps must be between 0 and 10000");
        let fee_recipient = fee_recipient.unwrap_or_else(|| creator.clone());
        if let Some(i) = &icon {
            require!(i.len() <= MAX_ICON_LEN, "Image is too large (max 12 KB)");
        }
        if let Some(d) = &description {
            require!(d.len() <= MAX_DESC_LEN, "Description is too long (max 280 characters)");
        }
        check_link(&website, "Website");
        check_link(&twitter, "X link");
        check_link(&telegram, "Telegram link");
        let r = graduation_threshold.0;
        let v = virtual_near.0;
        require!(r > 0 && v > 0, "Curve parameters must be positive");

        // Pick virtual token reserve so that selling exactly CURVE_SUPPLY raises exactly r:
        // v * t0 = (v + r) * (t0 - C)  =>  t0 = C * (v + r) / r
        let virtual_tokens = mul_div(CURVE_SUPPLY, v + r, r);

        let me = env::current_account_id();
        let mut token = FungibleToken::new(StorageKey::Token);
        token.internal_register_account(&me);
        token.internal_deposit(&me, TOTAL_SUPPLY);
        FtMint {
            owner_id: &me,
            amount: U128(TOTAL_SUPPLY),
            memo: Some("Fixed supply. Minted once, no mint function exists."),
        }
        .emit();

        let mut this = Self {
            token,
            name,
            symbol,
            icon,
            description,
            website,
            twitter,
            telegram,
            dev_buy_tokens: 0,
            creator,
            factory: env::predecessor_account_id(),
            buy_tax_bps,
            sell_tax_bps,
            burn_bps,
            fee_recipient,
            virtual_near: v,
            virtual_tokens,
            near_reserve: 0,
            curve_tokens_left: CURVE_SUPPLY,
            graduation_threshold: r,
            graduated: false,
            ref_contract,
            wrap_contract,
            lp_near: 0,
            lp_tokens: 0,
            migration_step: 0,
            migration_busy: false,
            migrate_caller: None,
            pool_id: None,
            trades: Vec::new(),
        };

        let dev = initial_buy.map(|v| v.0).unwrap_or(0);
        if dev > 0 {
            this.dev_buy(dev);
        }
        this
    }

    /// The creator's buy at launch, paid with NEAR the factory forwarded to this account.
    /// No tax, capped at 5% of supply, and never fills the curve. Unused NEAR is refunded.
    fn dev_buy(&mut self, amount: u128) {
        let me = env::current_account_id();
        let creator = self.creator.clone();
        if !self.token.accounts.contains_key(&creator) {
            self.token.internal_register_account(&creator);
        }
        let room = self.graduation_threshold - self.near_reserve - 1;
        let mut net = amount.min(room);
        let (x, y) = (self.x(), self.y());
        let mut out = mul_div(y, net, x + net);
        if out > MAX_DEV_TOKENS {
            out = MAX_DEV_TOKENS;
            net = mul_div_ceil(x, out, y - out).min(net);
        }
        out = out.min(self.curve_tokens_left);
        let refund = amount - net;

        if out > 0 {
            self.near_reserve += net;
            self.curve_tokens_left -= out;
            self.dev_buy_tokens = out;
            self.token.internal_transfer(&me, &creator, out, Some("SafeNear dev buy".into()));
            self.record("buy", creator.clone(), net, out);
            log!("DEV BUY: creator bought {} tokens for {} yoctoNEAR", out, net);
        }
        if refund > 0 {
            Promise::new(creator).transfer(NearToken::from_yoctonear(refund));
        }
    }

    /* ------------------------------ curve math ------------------------------ */

    fn x(&self) -> u128 {
        self.virtual_near + self.near_reserve
    }

    fn y(&self) -> u128 {
        self.virtual_tokens - (CURVE_SUPPLY - self.curve_tokens_left)
    }

    /// Splits a buy deposit into (gross used, tax, net into curve, refund).
    /// Caps the buy so the curve never goes past the graduation threshold.
    fn split_buy(&self, deposit: u128) -> (u128, u128, u128, u128) {
        let room = self.graduation_threshold - self.near_reserve;
        let bps = self.buy_tax_bps as u128;
        let max_gross = (room * BPS + (BPS - bps) - 1) / (BPS - bps); // ceil
        let gross = deposit.min(max_gross);
        let refund = deposit - gross;
        let net = (gross - gross * bps / BPS).min(room);
        let tax = gross - net;
        (gross, tax, net, refund)
    }

    fn tokens_out(&self, net_in: u128) -> u128 {
        if self.near_reserve + net_in >= self.graduation_threshold {
            return self.curve_tokens_left; // last buy takes whatever is left
        }
        mul_div(self.y(), net_in, self.x() + net_in).min(self.curve_tokens_left)
    }

    fn near_out(&self, tokens_in: u128) -> u128 {
        mul_div(self.x(), tokens_in, self.y() + tokens_in).min(self.near_reserve)
    }

    /// Reports activity to the factory for testnet points. Fire-and-forget: a failure there
    /// never affects the trade here.
    fn report(&self, account_id: AccountId, kind: &str, near_amount: u128) {
        ext_factory::ext(self.factory.clone())
            .with_static_gas(tgas(10))
            .record_event(account_id, kind.to_string(), U128(near_amount));
    }

    /// (to fee wallet, kept in curve)
    fn split_tax(&self, tax: u128) -> (u128, u128) {
        let burned = tax * self.burn_bps as u128 / BPS;
        (tax - burned, burned)
    }

    fn record(&mut self, side: &str, account_id: AccountId, near_amount: u128, token_amount: u128) {
        if self.trades.len() >= MAX_TRADES {
            self.trades.remove(0);
        }
        self.trades.push(Trade {
            side: side.to_string(),
            account_id,
            near_amount: U128(near_amount),
            token_amount: U128(token_amount),
            timestamp_ms: U64(env::block_timestamp_ms()),
        });
    }

    /* -------------------------------- views -------------------------------- */

    pub fn get_curve(&self) -> CurveView {
        CurveView {
            near_reserve: U128(self.near_reserve),
            graduation_threshold: U128(self.graduation_threshold),
            graduated: self.graduated,
            tax_bps: self.buy_tax_bps.max(self.sell_tax_bps),
            buy_tax_bps: self.buy_tax_bps,
            sell_tax_bps: self.sell_tax_bps,
            burn_bps: self.burn_bps,
            fee_recipient: self.fee_recipient.clone(),
            creator: self.creator.clone(),
            price: U128(mul_div(self.x(), ONE_TOKEN, self.y().max(1))),
            pool_id: self.pool_id,
            curve_tokens_left: U128(self.curve_tokens_left),
            migration_step: self.migration_step,
            lp_locked: self.migration_step >= DONE,
            fixed_supply: true,
            max_tax_bps: MAX_TAX_BPS,
        }
    }

    pub fn get_buy_quote(&self, near_in: U128) -> U128 {
        if self.graduated {
            return U128(0);
        }
        let (_, _, net, _) = self.split_buy(near_in.0);
        U128(self.tokens_out(net))
    }

    pub fn get_sell_quote(&self, tokens_in: U128) -> U128 {
        if self.graduated {
            return U128(0);
        }
        let gross = self.near_out(tokens_in.0);
        U128(gross - gross * self.sell_tax_bps as u128 / BPS)
    }

    pub fn get_recent_trades(&self, limit: Option<u32>) -> Vec<Trade> {
        let n = limit.unwrap_or(25) as usize;
        self.trades.iter().rev().take(n).cloned().collect()
    }

    pub fn get_info(&self) -> InfoView {
        InfoView {
            icon: self.icon.clone(),
            description: self.description.clone(),
            website: self.website.clone(),
            twitter: self.twitter.clone(),
            telegram: self.telegram.clone(),
            dev_buy_tokens: U128(self.dev_buy_tokens),
        }
    }

    pub fn get_factory(&self) -> AccountId {
        self.factory.clone()
    }

    /* ------------------------------- trading ------------------------------- */

    /// Buy tokens with the attached NEAR. Any NEAR above the graduation threshold is refunded.
    /// The buyer must be registered (storage_deposit) first; the frontend batches this.
    #[payable]
    pub fn buy(&mut self, min_tokens_out: U128) -> U128 {
        require!(!self.graduated, "This token graduated. Trade it on Ref Finance.");
        let buyer = env::predecessor_account_id();
        require!(
            self.token.accounts.contains_key(&buyer),
            "Register first with storage_deposit"
        );
        let deposit = env::attached_deposit().as_yoctonear();
        require!(deposit >= MIN_BUY, "Minimum buy is 0.01 NEAR");

        let (_gross, tax, net, refund) = self.split_buy(deposit);
        require!(net > 0, "Curve is full");
        let out = self.tokens_out(net);
        require!(out > 0, "Amount too small");
        require!(out >= min_tokens_out.0, "Price moved past your slippage. Try again.");

        let me = env::current_account_id();
        let (to_wallet, burned) = self.split_tax(tax);
        // The burned share stays in the curve without minting tokens, so the price rises for everyone.
        self.near_reserve += net + burned;
        self.curve_tokens_left -= out;
        self.token.internal_transfer(&me, &buyer, out, Some("SafeNear buy".into()));
        self.record("buy", buyer.clone(), net, out);
        self.report(buyer.clone(), "buy", net);

        if to_wallet > 0 {
            Promise::new(self.fee_recipient.clone()).transfer(NearToken::from_yoctonear(to_wallet));
        }
        if refund > 0 {
            Promise::new(buyer).transfer(NearToken::from_yoctonear(refund));
        }

        if self.near_reserve >= self.graduation_threshold {
            self.graduated = true;
            self.lp_near = self.near_reserve;
            self.lp_tokens = LP_SUPPLY + self.curve_tokens_left;
            self.curve_tokens_left = 0;
            self.report(self.creator.clone(), "graduated", 0);
            log!(
                "GRADUATED: {} NEAR raised. Call migrate() to move liquidity to Ref Finance.",
                self.near_reserve
            );
        }
        U128(out)
    }

    /// Sell tokens back to the curve. Requires exactly 1 yoctoNEAR attached.
    #[payable]
    pub fn sell(&mut self, tokens_in: U128, min_near_out: U128) -> U128 {
        assert_one_yocto();
        require!(!self.graduated, "This token graduated. Trade it on Ref Finance.");
        let seller = env::predecessor_account_id();
        let amount = tokens_in.0;
        require!(amount > 0, "Amount must be positive");

        let gross = self.near_out(amount);
        let tax = gross * self.sell_tax_bps as u128 / BPS;
        let net = gross - tax;
        let (to_wallet, burned) = self.split_tax(tax);
        require!(net > 0, "Amount too small");
        require!(net >= min_near_out.0, "Price moved past your slippage. Try again.");

        let me = env::current_account_id();
        self.token.internal_transfer(&seller, &me, amount, Some("SafeNear sell".into()));
        self.near_reserve -= gross - burned;
        self.curve_tokens_left += amount;
        self.record("sell", seller.clone(), gross, amount);
        self.report(seller.clone(), "sell", gross);

        Promise::new(seller).transfer(NearToken::from_yoctonear(net));
        if to_wallet > 0 {
            Promise::new(self.fee_recipient.clone()).transfer(NearToken::from_yoctonear(to_wallet));
        }
        U128(net)
    }

    /* ----------------------------- graduation ----------------------------- */

    /// Moves liquidity to Ref Finance one step at a time. Anyone can call it after graduation.
    /// Call it repeatedly (about 7 times) until get_curve().lp_locked is true.
    /// Attach 300 Tgas. Each step only advances if the previous one succeeded, so a failed
    /// step can simply be retried.
    pub fn migrate(&mut self) -> Promise {
        require!(self.graduated, "Not graduated yet");
        require!(self.migration_step < DONE, "Liquidity is already on Ref Finance and locked");
        require!(!self.migration_busy, "A migration step is already running");
        self.migration_busy = true;
        self.migrate_caller = Some(env::predecessor_account_id());

        let me = env::current_account_id();
        let refx = self.ref_contract.clone();
        let wrap = self.wrap_contract.clone();
        let step = self.migration_step;

        let p = match step {
            // 0: storage on Ref and wNEAR for this contract; register Ref on this token
            0 => {
                if !self.token.accounts.contains_key(&refx) {
                    self.token.internal_register_account(&refx);
                }
                ext_ref::ext(refx)
                    .with_attached_deposit(NearToken::from_millinear(100))
                    .with_static_gas(tgas(15))
                    .storage_deposit(Some(me.clone()), Some(false))
                    .and(
                        ext_wrap::ext(wrap)
                            .with_attached_deposit(NearToken::from_micronear(1_250))
                            .with_static_gas(tgas(15))
                            .storage_deposit(Some(me.clone()), Some(true)),
                    )
            }
            // 1: register both tokens in our Ref account
            1 => ext_ref::ext(refx)
                .with_attached_deposit(NearToken::from_yoctonear(1))
                .with_static_gas(tgas(15))
                .register_tokens(vec![wrap, me.clone()]),
            // 2: wrap the raised NEAR into wNEAR
            2 => ext_wrap::ext(wrap)
                .with_attached_deposit(NearToken::from_yoctonear(self.lp_near))
                .with_static_gas(tgas(15))
                .near_deposit(),
            // 3: deposit wNEAR into Ref
            3 => ext_wrap::ext(wrap)
                .with_attached_deposit(NearToken::from_yoctonear(1))
                .with_static_gas(tgas(80))
                .ft_transfer_call(refx, U128(self.lp_near), None, String::new()),
            // 4: deposit our LP token allocation into Ref (we are the token contract)
            4 => {
                self.token
                    .internal_transfer(&me, &refx, self.lp_tokens, Some("SafeNear LP".into()));
                ext_ref::ext(refx)
                    .with_static_gas(tgas(40))
                    .ft_on_transfer(me.clone(), U128(self.lp_tokens), String::new())
            }
            // 5: create the pool
            5 => ext_ref::ext(refx)
                .with_attached_deposit(NearToken::from_millinear(100))
                .with_static_gas(tgas(25))
                .add_simple_pool(vec![wrap, me.clone()], REF_POOL_FEE),
            // 6: add liquidity; LP shares stay in this contract's Ref account forever
            _ => ext_ref::ext(refx)
                .with_attached_deposit(NearToken::from_millinear(50))
                .with_static_gas(tgas(40))
                .add_liquidity(
                    self.pool_id.expect("No pool yet"),
                    vec![U128(self.lp_near), U128(self.lp_tokens)],
                    None,
                ),
        };
        p.then(Self::ext(me).with_static_gas(tgas(40)).on_migrate_step(step))
    }

    #[private]
    pub fn on_migrate_step(&mut self, step: u8) -> bool {
        self.migration_busy = false;
        let mut ok = true;
        let mut last: Vec<u8> = Vec::new();
        for i in 0..env::promise_results_count() {
            match env::promise_result(i) {
                PromiseResult::Successful(v) => last = v,
                _ => ok = false,
            }
        }
        let me = env::current_account_id();
        let refx = self.ref_contract.clone();

        match step {
            3 if ok => {
                let used: U128 = serde_json::from_slice(&last).unwrap_or(U128(0));
                ok = used.0 == self.lp_near;
            }
            4 => {
                // Ref returns the unused amount; anything unused (or everything on failure) comes back
                let unused: u128 = if ok {
                    serde_json::from_slice::<U128>(&last).map(|u| u.0).unwrap_or(self.lp_tokens)
                } else {
                    self.lp_tokens
                };
                if unused > 0 {
                    let back = unused.min(self.token.ft_balance_of(refx.clone()).0);
                    if back > 0 {
                        self.token.internal_transfer(&refx, &me, back, Some("SafeNear LP refund".into()));
                    }
                    ok = false;
                }
            }
            5 if ok => match serde_json::from_slice::<u64>(&last) {
                Ok(id) => self.pool_id = Some(id),
                Err(_) => ok = false,
            },
            _ => {}
        }

        if ok {
            if let Some(caller) = self.migrate_caller.take() {
                self.report(caller, "migrate", 0);
            }
            self.migration_step = step + 1;
            if self.migration_step >= DONE {
                log!(
                    "LIQUIDITY LOCKED on Ref Finance pool {}. LP shares cannot be withdrawn.",
                    self.pool_id.unwrap_or_default()
                );
            } else {
                log!("Graduation step {} done. Call migrate() again.", step);
            }
        } else {
            log!("Graduation step {} failed. It is safe to call migrate() again.", step);
        }
        ok
    }
}

/* --------------------------- NEP-141 plumbing --------------------------- */

#[near]
impl FungibleTokenCore for Contract {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        self.token.ft_transfer(receiver_id, amount, memo)
    }

    #[payable]
    fn ft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        amount: U128,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<U128> {
        self.token.ft_transfer_call(receiver_id, amount, memo, msg)
    }

    fn ft_total_supply(&self) -> U128 {
        self.token.ft_total_supply()
    }

    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.token.ft_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenResolver for Contract {
    #[private]
    fn ft_resolve_transfer(
        &mut self,
        sender_id: AccountId,
        receiver_id: AccountId,
        amount: U128,
    ) -> U128 {
        let (used, burned) =
            self.token
                .internal_ft_resolve_transfer(&sender_id, receiver_id, amount);
        if burned > 0 {
            log!("Account @{} burned {}", sender_id, burned);
        }
        used.into()
    }
}

#[near]
impl StorageManagement for Contract {
    #[payable]
    fn storage_deposit(
        &mut self,
        account_id: Option<AccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        self.token.storage_deposit(account_id, registration_only)
    }

    #[payable]
    fn storage_withdraw(&mut self, amount: Option<NearToken>) -> StorageBalance {
        self.token.storage_withdraw(amount)
    }

    #[payable]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        let who = env::predecessor_account_id();
        require!(
            who != env::current_account_id() && who != self.ref_contract,
            "This account can't unregister"
        );
        self.token.internal_storage_unregister(force).is_some()
    }

    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        self.token.storage_balance_bounds()
    }

    fn storage_balance_of(&self, account_id: AccountId) -> Option<StorageBalance> {
        self.token.storage_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenMetadataProvider for Contract {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        FungibleTokenMetadata {
            spec: FT_METADATA_SPEC.to_string(),
            name: self.name.clone(),
            symbol: self.symbol.clone(),
            icon: self.icon.clone(),
            reference: None,
            reference_hash: None,
            decimals: DECIMALS,
        }
    }
}

/* -------------------------------- tests -------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::testing_env;

    const NEAR: u128 = 1_000_000_000_000_000_000_000_000;

    fn setup(tax_bps: u16) -> Contract {
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id("dash.safenear.testnet".parse().unwrap())
            .predecessor_account_id("safenear.testnet".parse().unwrap())
            .account_balance(NearToken::from_near(10));
        testing_env!(ctx.build());
        Contract::new(
            "Dash".into(),
            "DASH".into(),
            None,
            accounts(0),
            tax_bps,
            tax_bps,
            U128(10 * NEAR),
            U128(3 * NEAR),
            "ref-finance-101.testnet".parse().unwrap(),
            "wrap.testnet".parse().unwrap(),
            Some("Run fast".into()),
            Some("https://dash.example".into()),
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn as_user(user: AccountId, deposit: u128) {
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id("dash.safenear.testnet".parse().unwrap())
            .predecessor_account_id(user)
            .account_balance(NearToken::from_near(100))
            .attached_deposit(NearToken::from_yoctonear(deposit));
        testing_env!(ctx.build());
    }

    #[test]
    fn fixed_supply_minted_once() {
        let c = setup(100);
        assert_eq!(c.ft_total_supply().0, TOTAL_SUPPLY);
        assert_eq!(c.curve_tokens_left, CURVE_SUPPLY);
    }

    #[test]
    fn buy_then_sell_roundtrip_without_tax() {
        let mut c = setup(0);
        let user = accounts(1);
        c.token.internal_register_account(&user);
        as_user(user.clone(), 2 * NEAR);
        let got = c.buy(U128(0)).0;
        assert!(got > 0);
        assert_eq!(c.near_reserve, 2 * NEAR);
        as_user(user.clone(), 1);
        let back = c.sell(U128(got), U128(0)).0;
        // constant product returns the same NEAR (minus rounding)
        assert!(2 * NEAR - back < 1_000);
        assert_eq!(c.curve_tokens_left, CURVE_SUPPLY);
    }

    #[test]
    fn filling_the_curve_graduates_and_refunds_extra() {
        let mut c = setup(100);
        let user = accounts(1);
        c.token.internal_register_account(&user);
        as_user(user.clone(), 50 * NEAR);
        let got = c.buy(U128(0)).0;
        assert_eq!(got, CURVE_SUPPLY);
        assert!(c.graduated);
        assert_eq!(c.near_reserve, 10 * NEAR);
        assert_eq!(c.lp_tokens, LP_SUPPLY);
    }

    #[test]
    fn dev_buy_is_capped_at_five_percent() {
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id("dash.safenear.testnet".parse().unwrap())
            .predecessor_account_id("safenear.testnet".parse().unwrap())
            .account_balance(NearToken::from_near(20));
        testing_env!(ctx.build());
        let c = Contract::new(
            "Dash".into(), "DASH".into(), None, accounts(0), 100, 100,
            U128(10 * NEAR), U128(3 * NEAR),
            "ref-finance-101.testnet".parse().unwrap(), "wrap.testnet".parse().unwrap(),
            None, None, None, None, Some(U128(9 * NEAR)), None, None,
        );
        assert_eq!(c.dev_buy_tokens, MAX_DEV_TOKENS);
        assert_eq!(c.ft_balance_of(accounts(0)).0, MAX_DEV_TOKENS);
        assert!(!c.graduated);
        assert!(c.near_reserve < 9 * NEAR);
    }

    #[test]
    #[should_panic(expected = "https:// link")]
    fn links_must_be_https() {
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id("dash.safenear.testnet".parse().unwrap())
            .predecessor_account_id("safenear.testnet".parse().unwrap());
        testing_env!(ctx.build());
        Contract::new(
            "Dash".into(), "DASH".into(), None, accounts(0), 100, 100,
            U128(10 * NEAR), U128(3 * NEAR),
            "ref-finance-101.testnet".parse().unwrap(), "wrap.testnet".parse().unwrap(),
            None, Some("javascript:alert(1)".into()), None, None, None, None, None,
        );
    }

    #[test]
    #[should_panic(expected = "Tax above 4% a side")]
    fn tax_is_capped() {
        setup(401);
    }

    #[test]
    fn burned_tax_raises_price() {
        let mut ctx = VMContextBuilder::new();
        ctx.current_account_id("dash.safenear.testnet".parse().unwrap())
            .predecessor_account_id("safenear.testnet".parse().unwrap())
            .account_balance(NearToken::from_near(10));
        testing_env!(ctx.build());
        let mut c = Contract::new(
            "Dash".into(), "DASH".into(), None, accounts(0), 400, 400,
            U128(10 * NEAR), U128(3 * NEAR),
            "ref-finance-101.testnet".parse().unwrap(), "wrap.testnet".parse().unwrap(),
            None, None, None, None, None, Some(accounts(2)), Some(10_000),
        );
        let user = accounts(1);
        c.token.internal_register_account(&user);
        as_user(user.clone(), NEAR);
        let price_before = c.get_curve().price.0;
        c.buy(U128(0));
        // all 4% tax burned: the whole deposit ends up in the curve
        assert_eq!(c.near_reserve, NEAR);
        assert!(c.get_curve().price.0 > price_before);
        assert_eq!(c.get_curve().fee_recipient, accounts(2));
    }
}
