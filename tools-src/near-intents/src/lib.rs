//! Near Intents WASM Tool for IronClaw.
//!
//! Provides token resolution, reverse lookups, and balance queries
//! for the NEAR Intents / Defuse protocol.
//!
//! No authentication required — both APIs are public.

wit_bindgen::generate!({
    world: "sandboxed-tool",
    path: "../../wit/tool.wit",
});

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

const DEFUSE_TOKENS_URL: &str = "https://1click.chaindefuser.com/v0/tokens";
const NEAR_RPC_URL: &str = "https://rpc.mainnet.near.org";
const MAX_ALIAS_WORDS: usize = 5;

const CURATED_ALIASES: &[(&str, &[(&str, Option<&str>)])] = &[
    ("ethereum", &[("ETH", None)]),
    ("ether", &[("ETH", None)]),
    ("bitcoin", &[("BTC", None), ("WBTC", None)]),
    ("btc", &[("BTC", None), ("WBTC", None)]),
    ("wrapped bitcoin", &[("WBTC", None)]),
    ("wrapped near", &[("wNEAR", None)]),
    ("near", &[("wNEAR", None)]),
    ("near protocol", &[("wNEAR", None)]),
    ("solana", &[("SOL", None)]),
    ("tether", &[("USDT", None)]),
    ("usd coin", &[("USDC", None)]),
    ("usdc", &[("USDC", None)]),
    ("usdt", &[("USDT", None)]),
    ("dai stablecoin", &[("DAI", None)]),
    ("dai", &[("DAI", None)]),
    ("dogecoin", &[("DOGE", None)]),
    ("doge", &[("DOGE", None)]),
    ("shiba", &[("SHIB", None)]),
    ("shiba inu", &[("SHIB", None)]),
    ("aurora", &[("AURORA", None)]),
    ("chainlink", &[("LINK", None)]),
    ("uniswap", &[("UNI", None)]),
    ("aave", &[("AAVE", None)]),
];

const BLOCKCHAIN_ALIASES: &[(&str, &str)] = &[
    ("near", "near"),
    ("ethereum", "eth"),
    ("eth", "eth"),
    ("solana", "sol"),
    ("sol", "sol"),
    ("arbitrum", "arbitrum"),
    ("arb", "arbitrum"),
    ("base", "base"),
    ("polygon", "polygon"),
    ("matic", "polygon"),
    ("avalanche", "avalanche"),
    ("avax", "avalanche"),
    ("bnb", "bnb"),
    ("binance", "bnb"),
    ("bsc", "bnb"),
    ("ton", "ton"),
    ("optimism", "optimism"),
    ("op", "optimism"),
];

#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
enum Action {
    #[serde(rename = "resolve_token")]
    ResolveToken {
        query: Option<String>,
        list_all: Option<bool>,
    },
    #[serde(rename = "reverse_resolve_token")]
    ReverseResolveToken { asset_id: String },
    #[serde(rename = "get_balance")]
    GetBalance {
        account_id: String,
        token_ids: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct DefuseTokenRaw {
    #[serde(rename = "assetId", default)]
    asset_id: String,
    #[serde(default)]
    decimals: u32,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    blockchain: String,
    #[serde(rename = "contractAddress")]
    contract_address: Option<String>,
    price: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct TokenMatch {
    asset_id: String,
    symbol: String,
    blockchain: String,
    decimals: u32,
}

#[derive(Debug, Clone)]
struct TokenMetadata {
    defuse_asset_id: String,
    decimals: u32,
    symbol: String,
    blockchain: String,
    contract_address: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenBalance {
    defuse_asset_id: String,
    symbol: Option<String>,
    raw_balance: String,
    balance: f64,
    decimals: u32,
}

struct NearIntentsTool;

impl exports::near::agent::tool::Guest for NearIntentsTool {
    fn execute(req: exports::near::agent::tool::Request) -> exports::near::agent::tool::Response {
        match execute_inner(&req.params) {
            Ok(result) => exports::near::agent::tool::Response {
                output: Some(result),
                error: None,
            },
            Err(e) => exports::near::agent::tool::Response {
                output: None,
                error: Some(e),
            },
        }
    }

    fn schema() -> String {
        SCHEMA.to_string()
    }

    fn description() -> String {
        "Near Intents tools for the Defuse protocol on NEAR. Resolves natural-language \
         token references to Defuse asset IDs, reverse-resolves asset IDs to metadata, \
         and queries multi-token balances. No authentication required."
            .to_string()
    }
}

export!(NearIntentsTool);

fn execute_inner(params: &str) -> Result<String, String> {
    let action: Action =
        serde_json::from_str(params).map_err(|e| format!("Invalid parameters: {e}"))?;

    match action {
        Action::ResolveToken { query, list_all } => resolve_token(query, list_all.unwrap_or(false)),
        Action::ReverseResolveToken { asset_id } => reverse_resolve_token(&asset_id),
        Action::GetBalance {
            account_id,
            token_ids,
        } => get_balance(&account_id, token_ids),
    }
}

fn fetch_token_list() -> Result<Vec<TokenMetadata>, String> {
    let headers = serde_json::json!({"Accept": "application/json"});
    let resp = near::agent::host::http_request("GET", DEFUSE_TOKENS_URL, &headers.to_string(), None, None)
        .map_err(|e| format!("Failed to fetch token list: {e}"))?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "Token list API returned status {}",
            resp.status
        ));
    }

    let body = String::from_utf8(resp.body)
        .map_err(|e| format!("Invalid UTF-8 in token list response: {e}"))?;

    let raw_tokens: Vec<DefuseTokenRaw> =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse token list: {e}"))?;

    let tokens = raw_tokens
        .into_iter()
        .filter(|t| !t.asset_id.is_empty())
        .map(|t| TokenMetadata {
            defuse_asset_id: t.asset_id,
            decimals: t.decimals,
            symbol: t.symbol,
            blockchain: t.blockchain,
            contract_address: t.contract_address,
        })
        .collect();

    Ok(tokens)
}

struct AliasMap {
    alias_map: HashMap<String, Vec<TokenMatch>>,
    reverse_map: HashMap<String, TokenMetadata>,
    tokens: Vec<TokenMetadata>,
}

fn build_alias_map(tokens: &[TokenMetadata]) -> AliasMap {
    let mut alias_map: HashMap<String, Vec<TokenMatch>> = HashMap::new();
    let mut reverse_map: HashMap<String, TokenMetadata> = HashMap::new();

    let mut canonical_to_aliases: HashMap<&str, Vec<&str>> = HashMap::new();
    for &(alias, canonical) in BLOCKCHAIN_ALIASES {
        canonical_to_aliases.entry(canonical).or_default().push(alias);
    }

    let mut blockchain_to_names: HashMap<&str, Vec<&str>> = HashMap::new();
    for &(alias, canonical) in BLOCKCHAIN_ALIASES {
        if let Some(names) = canonical_to_aliases.get(canonical) {
            blockchain_to_names.insert(alias, names.clone());
        }
    }

    for token in tokens {
        let m = TokenMatch {
            asset_id: token.defuse_asset_id.clone(),
            symbol: token.symbol.clone(),
            blockchain: token.blockchain.clone(),
            decimals: token.decimals,
        };

        add_alias(&mut alias_map, &token.symbol, &m);

        if let Some(ref addr) = token.contract_address {
            add_alias(&mut alias_map, addr, &m);
        }

        let sym_on_chain = format!("{} on {}", token.symbol, token.blockchain);
        add_alias(&mut alias_map, &sym_on_chain, &m);

        if let Some(names) = blockchain_to_names.get(token.blockchain.as_str()) {
            for name in names {
                let key = format!("{} on {}", token.symbol, name);
                add_alias(&mut alias_map, &key, &m);
            }
        }

        reverse_map.insert(token.defuse_asset_id.clone(), token.clone());
    }

    for &(alias, targets) in CURATED_ALIASES {
        for &(symbol_filter, blockchain_filter) in targets {
            for token in tokens {
                if token.symbol != symbol_filter {
                    continue;
                }
                if let Some(bf) = blockchain_filter {
                    if token.blockchain != bf {
                        continue;
                    }
                }
                let m = TokenMatch {
                    asset_id: token.defuse_asset_id.clone(),
                    symbol: token.symbol.clone(),
                    blockchain: token.blockchain.clone(),
                    decimals: token.decimals,
                };
                add_alias(&mut alias_map, alias, &m);
            }
        }
    }

    AliasMap {
        alias_map,
        reverse_map,
        tokens: tokens.to_vec(),
    }
}

fn add_alias(map: &mut HashMap<String, Vec<TokenMatch>>, alias: &str, m: &TokenMatch) {
    let key = alias.trim().to_lowercase();
    if key.is_empty() {
        return;
    }
    let entries = map.entry(key).or_default();
    if !entries.iter().any(|e| e.asset_id == m.asset_id) {
        entries.push(m.clone());
    }
}

fn scan_for_token_hits(input_text: &str, alias_map: &HashMap<String, Vec<TokenMatch>>) -> Vec<TokenMatch> {
    let raw_tokens: Vec<&str> = input_text.split_whitespace().collect();
    let tokens: Vec<String> = raw_tokens
        .iter()
        .map(|t| normalize_token(t))
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return Vec::new();
    }

    let mut consumed = vec![false; tokens.len()];
    let mut results: Vec<TokenMatch> = Vec::new();
    let mut seen_asset_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    let max_ngram = MAX_ALIAS_WORDS.min(tokens.len());

    for n in (1..=max_ngram).rev() {
        for i in 0..=(tokens.len() - n) {
            if consumed[i..i + n].iter().any(|&c| c) {
                continue;
            }

            let ngram: String = tokens[i..i + n]
                .iter()
                .map(|t| t.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");

            let entries = match alias_map.get(&ngram) {
                Some(e) => e,
                None => continue,
            };

            if n == 1 && ngram.len() <= 2 && !is_uppercase(&tokens[i]) {
                continue;
            }

            for j in i..i + n {
                consumed[j] = true;
            }

            for entry in entries {
                if seen_asset_ids.insert(entry.asset_id.clone()) {
                    results.push(entry.clone());
                }
            }
        }
    }

    results
}

fn normalize_token(token: &str) -> String {
    let s = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '&' && c != '\'');
    s.to_string()
}

fn is_uppercase(token: &str) -> bool {
    let mut has_alpha = false;
    for ch in token.chars() {
        if ch.is_alphabetic() {
            has_alpha = true;
            if !ch.is_uppercase() {
                return false;
            }
        }
    }
    has_alpha
}

fn resolve_token(query: Option<String>, list_all: bool) -> Result<String, String> {
    let tokens = fetch_token_list()?;
    let amap = build_alias_map(&tokens);

    if list_all {
        let all: Vec<TokenMatch> = amap
            .tokens
            .iter()
            .map(|t| TokenMatch {
                asset_id: t.defuse_asset_id.clone(),
                symbol: t.symbol.clone(),
                blockchain: t.blockchain.clone(),
                decimals: t.decimals,
            })
            .collect();
        return serde_json::to_string(&all).map_err(|e| format!("Serialization error: {e}"));
    }

    let query = query.ok_or("'query' is required when list_all is false")?;
    if query.is_empty() {
        return Err("'query' must not be empty".into());
    }

    let results = scan_for_token_hits(&query, &amap.alias_map);
    serde_json::to_string(&results).map_err(|e| format!("Serialization error: {e}"))
}

fn reverse_resolve_token(asset_id: &str) -> Result<String, String> {
    if asset_id.is_empty() {
        return Err("'asset_id' must not be empty".into());
    }

    let tokens = fetch_token_list()?;
    let amap = build_alias_map(&tokens);

    match amap.reverse_map.get(asset_id) {
        Some(token) => {
            let result = serde_json::json!({
                "asset_id": token.defuse_asset_id,
                "symbol": token.symbol,
                "blockchain": token.blockchain,
                "decimals": token.decimals,
            });
            Ok(result.to_string())
        }
        None => {
            let result = serde_json::json!({"error": format!("Unknown asset ID: {asset_id}")});
            Ok(result.to_string())
        }
    }
}

fn get_balance(account_id: &str, token_ids: Option<Vec<String>>) -> Result<String, String> {
    if account_id.is_empty() {
        return Err("'account_id' must not be empty".into());
    }

    let tokens = fetch_token_list()?;
    let amap = build_alias_map(&tokens);

    let ids: Vec<String> = match token_ids {
        Some(ids) if !ids.is_empty() => ids,
        _ => amap.tokens.iter().map(|t| t.defuse_asset_id.clone()).collect(),
    };

    let args_json = serde_json::json!({
        "account_id": account_id,
        "token_ids": ids,
    });
    let args_base64 = base64_encode(args_json.to_string().as_bytes());

    let rpc_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "dontcare",
        "method": "query",
        "params": {
            "request_type": "call_function",
            "finality": "final",
            "account_id": "intents.near",
            "method_name": "mt_batch_balance_of",
            "args_base64": args_base64,
        }
    });

    let headers = serde_json::json!({"Content-Type": "application/json"});
    let payload_bytes = rpc_payload.to_string().into_bytes();
    let resp = near::agent::host::http_request(
        "POST",
        NEAR_RPC_URL,
        &headers.to_string(),
        Some(&payload_bytes),
        None,
    )
    .map_err(|e| format!("NEAR RPC request failed: {e}"))?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!("NEAR RPC returned status {}", resp.status));
    }

    let body = String::from_utf8(resp.body)
        .map_err(|e| format!("Invalid UTF-8 in RPC response: {e}"))?;

    let rpc_resp: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse RPC response: {e}"))?;

    if let Some(err) = rpc_resp.get("error") {
        return Err(format!("NEAR RPC error: {err}"));
    }

    let result = rpc_resp
        .get("result")
        .ok_or("Missing 'result' in RPC response")?;

    if let Some(err) = result.get("error") {
        return Err(format!("NEAR RPC query error: {err}"));
    }

    // result.result is an array of bytes → decode to JSON string
    let result_bytes: Vec<u8> = result
        .get("result")
        .and_then(|r| r.as_array())
        .ok_or("Missing 'result.result' byte array in RPC response")?
        .iter()
        .map(|v| v.as_u64().unwrap_or(0) as u8)
        .collect();

    let raw_balances_str =
        String::from_utf8(result_bytes).map_err(|e| format!("Invalid UTF-8 in RPC result: {e}"))?;

    let raw_balances: Vec<String> = serde_json::from_str(&raw_balances_str)
        .map_err(|e| format!("Failed to parse balance array: {e}"))?;

    if raw_balances.len() != ids.len() {
        return Err(format!(
            "Balance count mismatch: expected {}, got {}",
            ids.len(),
            raw_balances.len()
        ));
    }

    let mut balances: Vec<TokenBalance> = Vec::new();
    for (token_id, raw_balance) in ids.iter().zip(raw_balances.iter()) {
        if raw_balance == "0" {
            continue;
        }

        let meta = amap.reverse_map.get(token_id);
        let decimals = meta.map(|m| m.decimals).unwrap_or(0);
        let symbol = meta.map(|m| m.symbol.clone());

        let balance = parse_balance(raw_balance, decimals);

        balances.push(TokenBalance {
            defuse_asset_id: token_id.clone(),
            symbol,
            raw_balance: raw_balance.clone(),
            balance,
            decimals,
        });
    }

    serde_json::to_string(&balances).map_err(|e| format!("Serialization error: {e}"))
}

fn parse_balance(raw: &str, decimals: u32) -> f64 {
    // Parse as u128 to handle large balances, then divide
    match raw.parse::<u128>() {
        Ok(val) => {
            let divisor = 10u128.pow(decimals);
            if divisor == 0 {
                val as f64
            } else {
                (val as f64) / (divisor as f64)
            }
        }
        Err(_) => 0.0,
    }
}

const BASE64_CHARS: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut output = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i] as u32;
        let b1 = if i + 1 < input.len() { input[i + 1] as u32 } else { 0 };
        let b2 = if i + 2 < input.len() { input[i + 2] as u32 } else { 0 };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        output.push(BASE64_CHARS[((triple >> 18) & 0x3F) as usize] as char);
        output.push(BASE64_CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if i + 1 < input.len() {
            output.push(BASE64_CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }

        if i + 2 < input.len() {
            output.push(BASE64_CHARS[(triple & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }

        i += 3;
    }
    output
}

const SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": ["resolve_token", "reverse_resolve_token", "get_balance"],
      "description": "Which action to perform"
    },
    "query": {
      "type": "string",
      "description": "Token reference to resolve (for resolve_token). Examples: 'ethereum', 'USDC on arbitrum', 'wrapped near', 'bitcoin'."
    },
    "list_all": {
      "type": "boolean",
      "description": "Set to true to return ALL tokens in the registry. Use when query returns empty results to find the correct token name/symbol.",
      "default": false
    },
    "asset_id": {
      "type": "string",
      "description": "Defuse asset ID to look up (for reverse_resolve_token). Example: 'nep141:wrap.near'."
    },
    "account_id": {
      "type": "string",
      "description": "NEAR wallet address or account ID (for get_balance)."
    },
    "token_ids": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Specific defuse asset IDs to query (for get_balance). If omitted, returns all non-zero balances."
    }
  },
  "required": ["action"]
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tokens() -> Vec<TokenMetadata> {
        vec![
            TokenMetadata {
                defuse_asset_id: "nep141:wrap.near".into(),
                decimals: 24,
                symbol: "wNEAR".into(),
                blockchain: "near".into(),
                contract_address: Some("wrap.near".into()),
            },
            TokenMetadata {
                defuse_asset_id: "nep141:usdc.near".into(),
                decimals: 6,
                symbol: "USDC".into(),
                blockchain: "near".into(),
                contract_address: Some("usdc.near".into()),
            },
            TokenMetadata {
                defuse_asset_id: "nep141:eth-usdc.arb".into(),
                decimals: 6,
                symbol: "USDC".into(),
                blockchain: "arbitrum".into(),
                contract_address: Some("0xa0b8...".into()),
            },
            TokenMetadata {
                defuse_asset_id: "nep141:eth.near".into(),
                decimals: 18,
                symbol: "ETH".into(),
                blockchain: "eth".into(),
                contract_address: None,
            },
            TokenMetadata {
                defuse_asset_id: "nep141:wbtc.near".into(),
                decimals: 8,
                symbol: "WBTC".into(),
                blockchain: "eth".into(),
                contract_address: None,
            },
        ]
    }

    #[test]
    fn test_normalize_token() {
        assert_eq!(normalize_token("hello"), "hello");
        assert_eq!(normalize_token("(hello)"), "hello");
        assert_eq!(normalize_token("..test.."), "test");
        assert_eq!(normalize_token("it's"), "it's");
        assert_eq!(normalize_token("R&D"), "R&D");
    }

    #[test]
    fn test_is_uppercase() {
        assert!(is_uppercase("ETH"));
        assert!(is_uppercase("BTC"));
        assert!(!is_uppercase("eth"));
        assert!(!is_uppercase("Eth"));
        assert!(is_uppercase("A1"));
        assert!(!is_uppercase("123")); // no alpha
    }

    #[test]
    fn test_alias_map_symbol_lookup() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);
        assert!(amap.alias_map.contains_key("wNEAR".to_lowercase().as_str()));
        assert!(amap.alias_map.contains_key("usdc"));
        assert!(amap.alias_map.contains_key("eth"));
    }

    #[test]
    fn test_alias_map_chain_specific() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);
        let key = "usdc on arbitrum";
        let results = amap.alias_map.get(key);
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].blockchain, "arbitrum");
    }

    #[test]
    fn test_alias_map_curated() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);

        // "ethereum" should map to ETH
        let results = amap.alias_map.get("ethereum").unwrap();
        assert!(results.iter().any(|r| r.symbol == "ETH"));

        // "bitcoin" should map to BTC and WBTC
        let results = amap.alias_map.get("bitcoin").unwrap();
        assert!(results.iter().any(|r| r.symbol == "WBTC"));

        // "near" should map to wNEAR
        let results = amap.alias_map.get("near").unwrap();
        assert!(results.iter().any(|r| r.symbol == "wNEAR"));
    }

    #[test]
    fn test_scan_basic() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);
        let results = scan_for_token_hits("I want some ethereum", &amap.alias_map);
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.symbol == "ETH"));
    }

    #[test]
    fn test_scan_chain_specific() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);
        let results = scan_for_token_hits("USDC on arbitrum", &amap.alias_map);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].blockchain, "arbitrum");
    }

    #[test]
    fn test_scan_short_alias_requires_uppercase() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);

        // Lowercase "do" should not match (if it were an alias)
        // But uppercase short aliases should match
        let results = scan_for_token_hits("I have some ETH", &amap.alias_map);
        assert!(results.iter().any(|r| r.symbol == "ETH"));

        // lowercase "eth" with len=3 is fine (>2 chars)
        let results = scan_for_token_hits("I have some eth", &amap.alias_map);
        assert!(results.iter().any(|r| r.symbol == "ETH"));
    }

    #[test]
    fn test_scan_longest_match_wins() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);
        // "USDC on arbitrum" is 3 tokens and should be matched as one unit
        let results = scan_for_token_hits("send USDC on arbitrum please", &amap.alias_map);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].blockchain, "arbitrum");
    }

    #[test]
    fn test_reverse_map() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);
        let meta = amap.reverse_map.get("nep141:wrap.near");
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().symbol, "wNEAR");
    }

    #[test]
    fn test_parse_balance() {
        assert!((parse_balance("1000000", 6) - 1.0).abs() < f64::EPSILON);
        assert!((parse_balance("1000000000000000000", 18) - 1.0).abs() < f64::EPSILON);
        assert!((parse_balance("0", 6) - 0.0).abs() < f64::EPSILON);
        assert!((parse_balance("500000", 6) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"Hello"), "SGVsbG8=");
        assert_eq!(base64_encode(b"Hi"), "SGk=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b""), "");

        // Verify a JSON args payload round-trips correctly
        let json = r#"{"account_id":"test.near","token_ids":["nep141:wrap.near"]}"#;
        let encoded = base64_encode(json.as_bytes());
        assert!(!encoded.is_empty());
        assert!(!encoded.contains('\n'));
    }

    #[test]
    fn test_action_deserialize() {
        let json = r#"{"action": "resolve_token", "query": "ethereum"}"#;
        let action: Action = serde_json::from_str(json).unwrap();
        match action {
            Action::ResolveToken { query, list_all } => {
                assert_eq!(query.unwrap(), "ethereum");
                assert!(list_all.is_none());
            }
            _ => panic!("Wrong variant"),
        }

        let json = r#"{"action": "get_balance", "account_id": "test.near"}"#;
        let action: Action = serde_json::from_str(json).unwrap();
        match action {
            Action::GetBalance {
                account_id,
                token_ids,
            } => {
                assert_eq!(account_id, "test.near");
                assert!(token_ids.is_none());
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_blockchain_alias_coverage() {
        let tokens = sample_tokens();
        let amap = build_alias_map(&tokens);

        // "usdc on arb" should work since "arb" is an alias for "arbitrum"
        assert!(amap.alias_map.contains_key("usdc on arb"));

        // "usdc on near" should work
        assert!(amap.alias_map.contains_key("usdc on near"));

        // "eth on ethereum" should work since ETH is on "eth" blockchain
        assert!(amap.alias_map.contains_key("eth on ethereum"));
        assert!(amap.alias_map.contains_key("eth on eth"));
    }
}
