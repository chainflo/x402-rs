//! Dynamic pricing example with x402-axum
//!
//! This example demonstrates an implementation of dynamic pricing using x402-axum.
//!
//! ## ⚠️  Production Security Considerations:
//! 
//! This example is simplified for demonstration. In production, you SHOULD take into account:
//! - Proper quote authentication
//! - Proper quote expiry
//! - Proper only-once semantics for quotes

use std::collections::HashMap;
use std::sync::{Arc};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{Router, routing::{get, post}, extract::{State}, response::IntoResponse, Json};
use dotenvy::dotenv;
use std::env;
use http::{HeaderMap, StatusCode, Uri};
use serde::Deserialize;
use url::Url;
use uuid::Uuid;

use x402_axum::X402Middleware;
use x402_axum::price::IntoPriceTag;
use x402_rs::network::{Network, USDCDeployment};
use x402_rs::types::{MoneyAmount, PaymentRequirements, Scheme};
use x402_rs::address_evm;

#[derive(Clone)]
struct QuoteInfo {
    amount: String,
    client_id: String, // Client ID for identifying the client that requested the quote
    expires_at: u64,      // Unix timestamp
    used: bool,           // Track if quote has been used
}

#[derive(Clone, Default)]
struct AppState {
    // In-memory quote store: quote_id -> QuoteInfo
    quotes: Arc<tokio::sync::Mutex<HashMap<String, QuoteInfo>>>,
}

#[derive(Deserialize)]
struct QuoteRequest {
    // a simple input that impacts price (e.g., numberOfFiles * unitPrice)
    number_of_files: u32,
}


async fn resolve_payment_requirements(
    headers: &HeaderMap,
    uri: &Uri,
    base_url: &Url,
    partial: &[x402_axum::layer::PaymentRequirementsNoResource],
    state: AppState,
) -> Result<Vec<x402_rs::types::PaymentRequirements>, x402_axum::layer::X402Error> {
    let quote_id = headers.get("X-Quote-Id").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    // In production, this should be a validated JWT or session token
    // otherwise clients can use quotes from other clients
    let client_id = headers.get("X-Client-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Build resource from base + URI
    let mut resource = base_url.clone();
    resource.set_path(uri.path());
    resource.set_query(uri.query());

    // If no quote id, reject with 402 showing the nominal requirements
    let quote_id = match quote_id {
        Some(q) => q,
        None => {
            // Return a 402 via X402Error by crafting it from nominal requirements
            let reqs = partial
                .iter()
                .map(|p| p.to_payment_requirements(resource.clone()))
                .collect::<Vec<_>>();
            return Err(x402_required(reqs));
        }
    };

    // Get current timestamp for validation
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Lookup quote info from the secure store
    let quote_info = {
        let store = state.quotes.lock().await;
        store.get(&quote_id).cloned()
    };

    let quote_info = match quote_info {
        Some(info) => info,
        None => {
            // Unknown quote -> present nominal requirements
            let reqs = partial
                .iter()
                .map(|p| p.to_payment_requirements(resource.clone()))
                .collect::<Vec<_>>();
            return Err(x402_required(reqs));
        }
    };

    // Security validations
    if quote_info.client_id != client_id {
        // Quote doesn't belong to this client
        let reqs = partial
            .iter()
            .map(|p| p.to_payment_requirements(resource.clone()))
            .collect::<Vec<_>>();
        return Err(x402_required(reqs));
    }

    if quote_info.expires_at < now {
        // Quote has expired
        let reqs = partial
            .iter()
            .map(|p| p.to_payment_requirements(resource.clone()))
            .collect::<Vec<_>>();
        return Err(x402_required(reqs));
    }

    if quote_info.used {
        // Quote has already been used
        let reqs = partial
            .iter()
            .map(|p| p.to_payment_requirements(resource.clone()))
            .collect::<Vec<_>>();
        return Err(x402_required(reqs));
    }

    // Mark quote as used
    {
        let mut store = state.quotes.lock().await;
        if let Some(info) = store.get_mut(&quote_id) {
            info.used = true;
        }
    }

    // Rewrite the max_amount_required with the quoted amount (token base units)
    let mut out = Vec::with_capacity(partial.len());
    for p in partial.iter() {
        let mut pr = p.to_payment_requirements(resource.clone());
        // amount_str is a human-readable money amount string; convert to token amount (USDC 6 decimals)
        if let Ok(m) = MoneyAmount::from_str(&quote_info.amount) {
            if let Ok(token_amount) = m.as_token_amount(6) {
                pr.max_amount_required = token_amount;
            }
        }
        pr.scheme = Scheme::Exact;
        out.push(pr);
    }
    Ok(out)
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let state = AppState::default();
    let resolver_state = state.clone();

    // Configure static parts of a price tag: token and payee
    let usdc = USDCDeployment::by_network(Network::BaseSepolia)
        .pay_to(address_evm!("0xBAc675C310721717Cd4A37F6cbeA1F081b1C2a07"));

    // Base middleware with token/payee; amount will be determined by resolver per request
    let facilitator_url = env::var("FACILITATOR_URL").unwrap_or_else(|_| "https://facilitator.x402.rs".to_string());
    let x402 = X402Middleware::try_from(facilitator_url).unwrap()
        .with_base_url(Url::parse("https://localhost:3001/").unwrap())
        .with_mime_type("application/json")
        // seed a small nominal amount to form partial requirements (replaced by resolver)
        .with_price_tag(usdc.amount("0.01").unwrap())
        .with_requirements_resolver(move |headers: &HeaderMap, uri: &Uri, base_url: &Url, partial| {
            let partial = partial.to_vec();
            let base = base_url.clone();
            let uri = uri.clone();
            let state = resolver_state.clone();
            Box::pin(async move {
                resolve_payment_requirements(headers, &uri, &base, &partial, state).await
            })
        });

    // Start cleanup task for expired quotes
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60)); // Run every minute
        loop {
            interval.tick().await;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            
            let mut store = cleanup_state.quotes.lock().await;
            store.retain(|_, quote_info| quote_info.expires_at > now);
        }
    });

    let app = Router::new()
        .route("/quote-resource", post(quote))
        .route("/resource", get(resource).layer(x402))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
    println!("Listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn quote(State(state): State<AppState>, Json(body): Json<QuoteRequest>) -> impl IntoResponse {
    // Example pricing: $0.01 per file
    let unit = MoneyAmount::try_from("0.01").unwrap();
    let total_money = MoneyAmount::try_from(body.number_of_files as f64 * 0.01f64).unwrap_or(unit);

    let quote_id = Uuid::new_v4().to_string();
    
    // Set quote to expire in 5 minutes
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() + 300; // 5 minutes

    {
        let mut store = state.quotes.lock().await;
        // Store secure quote info
        // In production, extract bearer token from request headers and validate it
        store.insert(quote_id.clone(), QuoteInfo {
            amount: total_money.to_string(),
            client_id: "demo-client".to_string(), // Demo client - use real auth in production
            expires_at,
            used: false,
        });
    }

    let res = serde_json::json!({
        "quote_id": quote_id,
        "amount": total_money.to_string()
    });
    (StatusCode::OK, Json(res))
}

async fn resource() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
}

// Helper to construct a 402 via the middleware error type
fn x402_required(accepts: Vec<PaymentRequirements>) -> x402_axum::layer::X402Error {
    x402_axum::layer::X402Error::payment_header_required(accepts)
}

