// Deploys res/safenear_factory.wasm to NEAR_ACCOUNT_ID on testnet and initializes it.
// Any problem is shown as a plain-language error on the GitHub Actions Summary page.
import fs from "node:fs";
import nearAPI from "near-api-js";
const { connect, keyStores, KeyPair } = nearAPI;

const fail = (title, msg) => {
  console.log(`::error title=${title}::${msg}`);
  console.error(`\n${title}: ${msg}\n`);
  process.exit(1);
};
const note = (msg) => console.log(`::notice title=SafeNear deploy::${msg}`);

const NETWORK = process.env.NETWORK === "mainnet" ? "mainnet" : "testnet";
const SUFFIX = NETWORK === "mainnet" ? ".near" : ".testnet";
const NET = {
  testnet: { rpcs: ["https://rpc.testnet.fastnear.com", "https://rpc.testnet.near.org", "https://test.rpc.fastnear.com"], ref: "ref-finance-101.testnet", wrap: "wrap.testnet", explorer: "https://testnet.nearblocks.io" },
  mainnet: { rpcs: ["https://rpc.mainnet.fastnear.com", "https://free.rpc.fastnear.com", "https://rpc.mainnet.near.org"], ref: "v2.ref-finance.near", wrap: "wrap.near", explorer: "https://nearblocks.io" },
}[NETWORK];
if (NETWORK === "mainnet" && (process.env.CONFIRM_MAINNET || "").trim() !== "DEPLOY MAINNET")
  fail("Mainnet not confirmed", 'This run targets MAINNET (real NEAR). Type DEPLOY MAINNET in the confirm box to continue.');
const accountId = (process.env.NEAR_ACCOUNT_ID || "").trim();
let privateKey = (process.env.NEAR_PRIVATE_KEY || "").trim();
const seedPhrase = (process.env.NEAR_SEED_PHRASE || "").trim().toLowerCase().replace(/\s+/g, " ");

// Use the 12-word seed phrase if there's no private key (same derivation path as MyNearWallet).
if (!privateKey && seedPhrase) {
  const words = seedPhrase.split(" ").length;
  if (words !== 12 && words !== 24) fail("Seed phrase looks wrong", `NEAR_SEED_PHRASE has ${words} words. It should be the 12 words from your wallet, separated by spaces.`);
  const { parseSeedPhrase } = await import("near-seed-phrase");
  privateKey = parseSeedPhrase(seedPhrase).secretKey;
  console.log("Using the key from NEAR_SEED_PHRASE.");
}
const owner = (process.env.NEAR_OWNER_ID || "").trim() || accountId;
const DO_DEPLOY = process.env.DO_DEPLOY === "true";
const DO_PUBLISH = process.env.DO_PUBLISH === "true";
const DO_CURVE = process.env.DO_CURVE === "true";
const CURVE_GRAD = Number(process.env.CURVE_GRAD || "100");
const CURVE_VIRTUAL = Number(process.env.CURVE_VIRTUAL || "30");
if (DO_CURVE) {
  if (!(CURVE_GRAD >= 1 && CURVE_GRAD <= 100000)) fail("Bad graduation amount", `graduation_near is "${process.env.CURVE_GRAD}". Use a number of NEAR, for example 100.`);
  if (!(CURVE_VIRTUAL > 0 && CURVE_VIRTUAL <= CURVE_GRAD * 5)) fail("Bad virtual reserve", `virtual_near is "${process.env.CURVE_VIRTUAL}". Use a number around 30% of graduation, for example 30 for 100.`);
}
if (!DO_DEPLOY && !DO_PUBLISH && !DO_CURVE) { console.log("Nothing to do (deploy and publish are both off)."); process.exit(0); }

// 1. Secrets present?
const SECRET = (n) => NETWORK === "mainnet" ? "MAINNET_" + n : "NEAR_" + n;
if (!accountId) fail(`Missing ${SECRET("ACCOUNT_ID")}`, `Add the secret ${SECRET("ACCOUNT_ID")} with your ${NETWORK} wallet name (ends in ${SUFFIX}).`);
if (!privateKey) fail("Missing key", `Add the secret ${SECRET("PRIVATE_KEY")} with your ${NETWORK} wallet's private key (or ${SECRET("SEED_PHRASE")} with its 12 words).`);
if (!accountId.endsWith(SUFFIX)) fail("Wrong account for this network", `The ${NETWORK} account is "${accountId}". It must end in ${SUFFIX}.`);

// 2. Key format
if (!privateKey.startsWith("ed25519:")) fail("Wrong key format", "NEAR_PRIVATE_KEY must start with ed25519: (copy the whole key, including ed25519:)");
const keyBody = privateKey.slice(8);
if (keyBody.length < 70) fail("That is the PUBLIC key", `NEAR_PRIVATE_KEY is only ${keyBody.length} characters after ed25519:. That's the public key. Export the PRIVATE key from your wallet (it's about 88 characters).`);
let keyPair;
try { keyPair = KeyPair.fromString(privateKey); }
catch { fail("Key can't be read", "NEAR_PRIVATE_KEY isn't a valid key. Copy it again from your wallet with no spaces or line breaks."); }
const publicKey = keyPair.getPublicKey().toString();

// 3. Connect
const keyStore = new keyStores.InMemoryKeyStore();
await keyStore.setKey(NETWORK, accountId, keyPair);
let near;
for (const nodeUrl of NET.rpcs) {
  try { near = await connect({ networkId: NETWORK, nodeUrl, keyStore }); await near.connection.provider.status(); break; }
  catch { near = null; }
}
if (!near) fail(`Can't reach NEAR ${NETWORK}`, `All ${NETWORK} RPC servers failed. Re-run the workflow in a few minutes.`);
console.log(`Network: ${NETWORK.toUpperCase()}`);
const account = await near.account(accountId);

// 4. Account exists + balance
let state;
try { state = await account.state(); }
catch { fail("Account not found", `"${accountId}" doesn't exist on ${NETWORK}. Check the spelling in ${SECRET("ACCOUNT_ID")} (copy it from your wallet).`); }
const balance = Number(BigInt(state.amount) / 10n ** 21n) / 1000;

// 5. Key belongs to this account?
const keys = await near.connection.provider.query({ request_type: "view_access_key_list", finality: "final", account_id: accountId });
const match = keys.keys.find((k) => k.public_key === publicKey);
if (!match) fail("Key doesn't match the account", `The key or seed phrase belongs to a different wallet than ${accountId}. Use the 12 words of the ${accountId} wallet itself.`);
if (match.access_key.permission !== "FullAccess") fail("Key has limited access", "This key can only call some contracts. Export the wallet's FULL ACCESS private key instead.");

const NEAR = (n) => (BigInt(Math.round(n * 1000)) * 10n ** 21n).toString();

if (DO_DEPLOY) {
// 6. Enough NEAR for the code storage?
const wasm = fs.readFileSync("res/safenear_factory.wasm");
const needed = (wasm.length / 100_000) + 1; // 1 NEAR per 100 KB plus buffer
if (balance < needed) fail(`Not enough ${NETWORK} NEAR`, `${accountId} has ${balance.toFixed(2)} NEAR but deploying needs about ${needed.toFixed(1)} NEAR.${NETWORK === "testnet" ? " Get more from near-faucet.io and re-run." : " Send more NEAR to it and re-run."}`);
console.log(`Account ${accountId}: ${balance.toFixed(3)} NEAR, key OK. Deploying ${(wasm.length / 1024).toFixed(0)} KB...`);

// 7. Deploy + init
const initArgs = {
  owner,
  creation_fee: NEAR(5),
  token_account_balance: NEAR(4.5),
  graduation_threshold: NEAR(DO_CURVE ? CURVE_GRAD : (NETWORK === "mainnet" ? 1000 : 100)),
  virtual_near: NEAR(DO_CURVE ? CURVE_VIRTUAL : (NETWORK === "mainnet" ? 300 : 30)),
  ref_contract: NET.ref,
  wrap_contract: NET.wrap,
};
try {
  const d = await account.deployContract(wasm);
  console.log("Deployed. Tx:", d.transaction.hash);
} catch (e) {
  fail("Deploy transaction failed", String(e?.message || e).replace(/\s+/g, " ").slice(0, 300));
}
try {
  const r = await account.functionCall({ contractId: accountId, methodName: "new", args: initArgs, gas: "100000000000000" });
  console.log("Initialized. Tx:", r.transaction.hash);
} catch (e) {
  const m = String(e?.message || e);
  if (/already been initialized|already initialized/i.test(m)) console.log("Already initialized, code updated only.");
  else fail("Setup (init) failed", m.replace(/\s+/g, " ").slice(0, 300));
}

note(`Factory code is live at ${accountId}.`);
}

// 8. Publish the token code globally -> cheap launches
if (DO_PUBLISH) {
  const view = async (method, args = {}) => {
    const r = await near.connection.provider.query({ request_type: "call_function", finality: "final", account_id: accountId, method_name: method, args_base64: Buffer.from(JSON.stringify(args)).toString("base64") });
    return JSON.parse(Buffer.from(r.result).toString());
  };
  let mode;
  try { mode = await view("get_launch_mode"); }
  catch { fail("Old factory code", "The factory doesn't have the cheap-launch update yet. Run again with BOTH Deploy and Publish checked."); }
  if (mode.global) {
    note(`Cheap launches are already on. Launch fee: ${(Number(BigInt(mode.creation_fee) / 10n ** 21n) / 1000)} NEAR.`);
  } else {
    const cost = Number(BigInt(mode.publish_cost) / 10n ** 21n) / 1000;
    const now = Number(BigInt((await account.state()).amount) / 10n ** 21n) / 1000;
    const need = cost + 1;
    console.log(`Token code: ${(mode.token_code_bytes / 1024).toFixed(0)} KB. Publishing burns ${cost.toFixed(2)} NEAR once. Balance: ${now.toFixed(2)} NEAR.`);
    if (now < need) fail(`Not enough ${NETWORK} NEAR to publish`, `Publishing the token code burns ${cost.toFixed(2)} NEAR once (10 NEAR per 100 KB). ${accountId} has ${now.toFixed(2)} NEAR; it needs about ${need.toFixed(1)}. Send more NEAR to it and run again with Publish checked.`);
    try {
      const r = await account.functionCall({ contractId: accountId, methodName: "publish_token_code", args: { creation_fee: NEAR(0.7), token_account_balance: NEAR(0.6) }, gas: "300000000000000" });
      console.log("Publish tx:", r.transaction.hash);
    } catch (e) { fail("Publishing failed", String(e?.message || e).replace(/\s+/g, " ").slice(0, 300)); }
    const after = await view("get_launch_mode");
    if (!after.global) fail("Publishing didn't switch on", `The publish transaction ran but cheap launches are still off. Check it on ${NET.explorer}.`);
    note(`Cheap launches are ON. Each launch now costs ${(Number(BigInt(after.creation_fee) / 10n ** 21n) / 1000)} NEAR (was 5 NEAR).`);
  }
}

// 9. Bonding curve for new launches
if (DO_CURVE) {
  try {
    await account.functionCall({ contractId: accountId, methodName: "set_config", args: { graduation_threshold: NEAR(CURVE_GRAD), virtual_near: NEAR(CURVE_VIRTUAL) }, gas: "30000000000000" });
  } catch (e) { fail("Curve update failed", String(e?.message || e).replace(/\s+/g, " ").slice(0, 300)); }
  const v = CURVE_VIRTUAL, r = CURVE_GRAD;
  const startMc = 1.25 * v * r / (v + r), endMc = 1.25 * r * (v + r) / v;
  note(`New launches bond at ${r} NEAR raised. Market cap ~${startMc.toFixed(0)} NEAR at start, ~${endMc.toFixed(0)} NEAR at graduation (${(endMc / startMc).toFixed(0)}x). Existing tokens keep their old curve.`);
}

console.log(`\nDone! Site: ?network=${NETWORK}&factory=${accountId}`);
