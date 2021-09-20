use std::borrow::Cow;
use std::collections::HashSet;
use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::io::Write;

use anoma::proto::Tx;
use anoma::types::address::Address;
use anoma::types::intent::{DecimalWrapper, Exchange, FungibleTokenIntent};
use anoma::types::key::ed25519::Signed;
use anoma::types::nft::NftToken;
use anoma::types::token;
use anoma::types::token::Amount;
use anoma::types::transaction::{
    pos, CreateNft, InitAccount, MintNft, UpdateVp,
};
use async_std::io::{self, WriteExt};
use borsh::BorshSerialize;
use jsonpath_lib as jsonpath;
use serde::{Deserialize, Serialize};
use tendermint_rpc::query::{EventType, Query};
use tendermint_rpc::Client;

use super::{rpc, signing};
use crate::cli::{args, safe_exit, Context};
use crate::client::tendermint_websocket_client::{
    hash_tx, Error, TendermintWebsocketClient, WebSocketAddress,
};
use crate::proto::services::rpc_service_client::RpcServiceClient;
use crate::proto::{services, RpcMessage};
use crate::wallet::Wallet;

const TX_INIT_ACCOUNT_WASM: &str = "wasm/tx_init_account.wasm";
const TX_CREATE_NFT: &str = "wasm/tx_create_nft.wasm";
const TX_UPDATE_VP_WASM: &str = "wasm/tx_update_vp.wasm";
const TX_TRANSFER_WASM: &str = "wasm/tx_transfer.wasm";
const VP_USER_WASM: &str = "wasm/vp_user.wasm";
const TX_MINT_NFT_TOKEN: &str = "wasm/tx_mint_nft_tokens.wasm";
const VP_NFT: &str = "wasm/vp_nft.wasm";

#[derive(Debug, Clone, Deserialize)]
pub struct NftDefinition {
    /// The source address
    pub owner: Address,
    /// The path to the vp code
    pub vp_path: Option<String>,
    /// the nft tokens
    pub tokens: Vec<NftToken>,
}

#[derive(Debug, Clone, Deserialize, BorshSerialize)]
pub struct ExchangeDefinition {
    /// The source address
    pub addr: Address,
    /// The token to be sold
    pub token_sell: Address,
    /// The minimum rate
    pub rate_min: DecimalWrapper,
    /// The maximum amount of token to be sold
    pub max_sell: Amount,
    /// The token to be bought
    pub token_buy: Address,
    /// The amount of token to be bought
    pub min_buy: Amount,
    // The path to the wasm vp code
    pub vp_path: Option<String>,
}

impl TryInto<Exchange> for ExchangeDefinition {
    type Error = &'static str;

    fn try_into(self) -> Result<Exchange, Self::Error> {
        let exchange_vp_code = self
            .vp_path
            .as_ref()
            .map(|path| {
                Some(
                    std::fs::read(path)
                        .expect("Expected a file at given code path"),
                )
            })
            .unwrap_or_else(|| None);

        Ok(Exchange {
            addr: self.addr,
            token_sell: self.token_sell,
            rate_min: self.rate_min,
            max_sell: self.max_sell,
            token_buy: self.token_buy,
            min_buy: self.min_buy,
            vp: exchange_vp_code,
        })
    }
}
const TX_BOND_WASM: &str = "wasm/tx_bond.wasm";
const TX_UNBOND_WASM: &str = "wasm/tx_unbond.wasm";
const TX_WITHDRAW_WASM: &str = "wasm/tx_withdraw.wasm";

pub async fn submit_custom(mut ctx: Context, args: args::TxCustom) {
    let tx_code = std::fs::read(args.code_path)
        .expect("Expected a file at given code path");
    let data = args.data_path.map(|data_path| {
        std::fs::read(data_path).expect("Expected a file at given data path")
    });
    let tx = Tx::new(tx_code, data);
    let tx = if let Some(signing_key) = args.signing_key {
        let signing_key = ctx.get_cached(signing_key);
        tx.sign(&signing_key)
    } else if let Some(signer) = args.signer {
        let signer = ctx.get(signer);
        let signing_key = signing::find_keypair(
            &mut ctx.wallet,
            &signer,
            args.tx.ledger_address.clone(),
        )
        .await;
        tx.sign(&signing_key)
    } else {
        // Unsigned tx
        tx
    };

    submit_tx(ctx, args.tx, tx).await
}

pub async fn submit_update_vp(mut ctx: Context, args: args::TxUpdateVp) {
    let source = ctx.get(args.addr);
    let keypair = signing::find_keypair(
        &mut ctx.wallet,
        &source,
        args.tx.ledger_address.clone(),
    )
    .await;

    let vp_code = std::fs::read(args.vp_code_path)
        .expect("Expected a file at given code path");
    let tx_code = std::fs::read(TX_UPDATE_VP_WASM)
        .expect("Expected a file at given code path");

    let update_vp = UpdateVp {
        addr: source,
        vp_code,
    };
    let data = update_vp.try_to_vec().expect(
        "Encoding transfer data to update a validity predicate shouldn't fail",
    );
    let tx = Tx::new(tx_code, Some(data)).sign(&keypair);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn submit_init_account(mut ctx: Context, args: args::TxInitAccount) {
    let source = ctx.get(args.source);
    let keypair = signing::find_keypair(
        &mut ctx.wallet,
        &source,
        args.tx.ledger_address.clone(),
    )
    .await;
    let public_key = ctx.get_cached(args.public_key);
    let vp_code = args
        .vp_code_path
        .map(|path| {
            std::fs::read(path).expect("Expected a file at given code path")
        })
        .unwrap_or_else(|| {
            std::fs::read(VP_USER_WASM)
                .expect("Expected a file at given code path")
        });
    let tx_code = std::fs::read(TX_INIT_ACCOUNT_WASM)
        .expect("Expected a file at given code path");

    let data = InitAccount {
        public_key,
        vp_code,
    };
    let data = data.try_to_vec().expect(
        "Encoding transfer data to initialize a new account shouldn't fail",
    );
    let tx = Tx::new(tx_code, Some(data)).sign(&keypair);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn mint_nft(mut ctx: Context, args: args::NftMint) {
    let signing_key = ctx.get_cached(args.signing_key);

    let file = File::open(&args.nft_data).expect("File must exist.");
    let nft_tokens: Vec<NftToken> =
        serde_json::from_reader(file).expect("JSON was not well-formatted");

    let data = MintNft {
        owner: args.nft_owner,
        address: args.nft_address,
        tokens: nft_tokens,
    };
    let data = data.try_to_vec().expect(
        "Encoding transfer data to initialize a new account shouldn't fail",
    );

    let tx_code = std::fs::read(TX_MINT_NFT_TOKEN)
        .expect("Expected a file at given code path");

    let tx = Tx::new(tx_code, Some(data)).sign(&signing_key);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn create_nft(mut ctx: Context, args: args::NftCreate) {
    let signing_key = ctx.get_cached(args.signing_key);

    let file = File::open(&args.nft_data).expect("File must exist.");
    let nft_definition: NftDefinition =
        serde_json::from_reader(file).expect("JSON was not well-formatted");

    let nft_vp_code = nft_definition
        .vp_path
        .map(|path| {
            std::fs::read(path).expect("Expected a file at given code path")
        })
        .unwrap_or_else(|| {
            std::fs::read(VP_NFT).expect("Expected a file at given code path")
        });

    let data = CreateNft {
        owner: nft_definition.owner,
        vp_code: nft_vp_code,
        tokens: nft_definition.tokens,
    };
    let data = data.try_to_vec().expect(
        "Encoding transfer data to initialize a new account shouldn't fail",
    );

    let tx_code = std::fs::read(TX_CREATE_NFT)
        .expect("Expected a file at given code path");

    let tx = Tx::new(tx_code, Some(data)).sign(&signing_key);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn gossip_intent(
    mut ctx: Context,
    args::Intent {
        node_addr,
        topic,
        signing_key,
        exchanges,
        ledger_address,
        to_stdout,
    }: args::Intent,
) {
    let mut signed_exchanges: HashSet<Signed<Exchange>> =
        HashSet::with_capacity(exchanges.len());
    for exchange in exchanges {
        let signed =
            sign_exchange(exchange, &mut ctx.wallet, ledger_address.clone())
                .await;
        signed_exchanges.insert(signed);
    }

    let signing_key = ctx.get_cached(signing_key);
    let signed_ft: Signed<FungibleTokenIntent> = Signed::new(
        &signing_key,
        FungibleTokenIntent {
            exchange: signed_exchanges,
        },
    );
    let data_bytes = signed_ft.try_to_vec().unwrap();

    if to_stdout {
        let mut out = std::io::stdout();
        out.write_all(&data_bytes).unwrap();
        out.flush().unwrap();
    } else {
        let node_addr = node_addr.expect(
            "Gossip node address must be defined to submit the intent to it.",
        );
        let topic = topic.expect(
            "The topic must be defined to submit the intent to a gossip node.",
        );
        let mut client = RpcServiceClient::connect(node_addr).await.unwrap();

        let intent = anoma::proto::Intent::new(data_bytes);
        let message: services::RpcMessage =
            RpcMessage::new_intent(intent, topic).into();
        let response = client
            .send_message(message)
            .await
            .expect("failed to send message and/or receive rpc response");
        println!("{:#?}", response);
    }
}

async fn sign_exchange(
    exchange: Exchange,
    wallet: &mut Wallet,
    ledger_address: tendermint::net::Address,
) -> Signed<Exchange> {
    let source_keypair =
        signing::find_keypair(wallet, &exchange.addr, ledger_address).await;
    Signed::new(&source_keypair, exchange.clone())
}

pub async fn subscribe_topic(
    _ctx: Context,
    args::SubscribeTopic { node_addr, topic }: args::SubscribeTopic,
) {
    let mut client = RpcServiceClient::connect(node_addr).await.unwrap();
    let message: services::RpcMessage = RpcMessage::new_topic(topic).into();
    let response = client
        .send_message(message)
        .await
        .expect("failed to send message and/or receive rpc response");
    println!("{:#?}", response);
}

pub async fn submit_transfer(mut ctx: Context, args: args::TxTransfer) {
    let source = ctx.get(args.source);
    let target = ctx.get(args.target);
    let token = ctx.get(args.token);
    let keypair = signing::find_keypair(
        &mut ctx.wallet,
        &source,
        args.tx.ledger_address.clone(),
    )
    .await;

    let _tx_code = std::fs::read(TX_TRANSFER_WASM).unwrap();

    let tx_code = std::fs::read(TX_TRANSFER_WASM).unwrap();
    let transfer = token::Transfer {
        source,
        target,
        token,
        amount: args.amount,
    };
    tracing::debug!("Transfer data {:?}", transfer);
    let data = transfer
        .try_to_vec()
        .expect("Encoding unsigned transfer shouldn't fail");
    let tx = Tx::new(tx_code, Some(data)).sign(&keypair);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn submit_bond(mut ctx: Context, args: args::Bond) {
    let validator = ctx.get(args.validator);
    let source = ctx.get_opt(args.source);
    let signer = source.as_ref().unwrap_or(&validator);
    let keypair = signing::find_keypair(
        &mut ctx.wallet,
        signer,
        args.tx.ledger_address.clone(),
    )
    .await;
    let tx_code = std::fs::read(TX_BOND_WASM).unwrap();

    let bond = pos::Bond {
        validator,
        amount: args.amount,
        source,
    };
    tracing::debug!("Bond data {:?}", bond);
    let data = bond.try_to_vec().expect("Encoding tx data shouldn't fail");
    let tx = Tx::new(tx_code, Some(data)).sign(&keypair);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn submit_unbond(mut ctx: Context, args: args::Unbond) {
    let validator = ctx.get(args.validator);
    let source = ctx.get_opt(args.source);
    let signer = source.as_ref().unwrap_or(&validator);
    let keypair = signing::find_keypair(
        &mut ctx.wallet,
        signer,
        args.tx.ledger_address.clone(),
    )
    .await;
    let tx_code = std::fs::read(TX_UNBOND_WASM).unwrap();

    let unbond = pos::Unbond {
        validator,
        amount: args.amount,
        source,
    };
    tracing::debug!("Unbond data {:?}", unbond);
    let data = unbond
        .try_to_vec()
        .expect("Encoding tx data shouldn't fail");
    let tx = Tx::new(tx_code, Some(data)).sign(&keypair);

    submit_tx(ctx, args.tx, tx).await
}

pub async fn submit_withdraw(mut ctx: Context, args: args::Withdraw) {
    let validator = ctx.get(args.validator);
    let source = ctx.get_opt(args.source);
    let signer = source.as_ref().unwrap_or(&validator);
    let keypair = signing::find_keypair(
        &mut ctx.wallet,
        signer,
        args.tx.ledger_address.clone(),
    )
    .await;
    let tx_code = std::fs::read(TX_WITHDRAW_WASM).unwrap();

    let withdraw = pos::Withdraw { validator, source };
    tracing::debug!("Withdraw data {:?}", withdraw);
    let data = withdraw
        .try_to_vec()
        .expect("Encoding tx data shouldn't fail");
    let tx = Tx::new(tx_code, Some(data)).sign(&keypair);

    submit_tx(ctx, args.tx, tx).await
}

async fn submit_tx(ctx: Context, args: args::Tx, tx: Tx) {
    let tx_bytes = tx.to_bytes();

    // NOTE: use this to print the request JSON body:

    // let request =
    // tendermint_rpc::endpoint::broadcast::tx_commit::Request::new(
    //     tx_bytes.clone().into(),
    // );
    // use tendermint_rpc::Request;
    // let request_body = request.into_json();
    // println!("HTTP request body: {}", request_body);

    if args.dry_run {
        rpc::dry_run_tx(&args.ledger_address, tx_bytes).await
    } else {
        match broadcast_tx(args.ledger_address.clone(), tx_bytes).await {
            Ok(result) => {
                save_initialized_accounts(ctx, args, result).await;
            }
            Err(err) => {
                eprintln!(
                    "Encountered error while broadcasting transaction: {}",
                    err
                );
                safe_exit(1)
            }
        }
    }
}

/// Save accounts initialized from a tx into the wallet, if any.
async fn save_initialized_accounts(
    ctx: Context,
    args: args::Tx,
    result: TxResponse,
) {
    let len = result.initialized_accounts.len();
    if len != 0 {
        // Store newly initialized account addresses in the wallet
        println!(
            "The transaction initialized {} new account{}",
            len,
            if len == 1 { "" } else { "s" }
        );
        // Store newly initialized account addresses in the wallet
        let mut wallet = ctx.wallet;
        for (ix, address) in result.initialized_accounts.iter().enumerate() {
            let encoded = address.encode();
            let mut added = false;
            while !added {
                let alias: Cow<str> = match &args.initialized_account_alias {
                    Some(initialized_account_alias) => {
                        if len == 1 {
                            // If there's only one account, use the
                            // alias as is
                            initialized_account_alias.into()
                        } else {
                            // If there're multiple accounts, use
                            // the alias as prefix, followed by
                            // index number
                            format!("{}{}", initialized_account_alias, ix)
                                .into()
                        }
                    }
                    None => {
                        print!("Choose an alias for {}: ", encoded);
                        io::stdout().flush().await.unwrap();
                        let mut alias = String::new();
                        io::stdin().read_line(&mut alias).await.unwrap();
                        alias.trim().to_owned().into()
                    }
                };
                added = if alias.is_empty() {
                    println!(
                        "Empty alias given, using {} as the alias.",
                        encoded
                    );
                    wallet.add_address(encoded.clone(), address.clone())
                } else {
                    let alias = alias.into_owned();
                    let added =
                        wallet.add_address(alias.clone(), address.clone());
                    if added {
                        println!(
                            "Added alias {} for address {}.",
                            alias, encoded
                        );
                    }
                    added
                }
            }
        }
        wallet.save().unwrap_or_else(|err| eprintln!("{}", err));
    }
}

pub async fn broadcast_tx(
    address: tendermint::net::Address,
    tx_bytes: Vec<u8>,
) -> Result<TxResponse, Error> {
    let mut client =
        TendermintWebsocketClient::open(WebSocketAddress::try_from(address)?)?;
    // It is better to subscribe to the transaction before it is broadcast
    //
    // Note that the `applied.hash` key comes from a custom event
    // created by the shell
    let query = Query::from(EventType::NewBlock)
        .and_eq("applied.hash", hash_tx(&tx_bytes).to_string());
    client.subscribe(query)?;
    println!(
        "Transaction added to mempool: {:?}",
        client
            .broadcast_tx_sync(tx_bytes.into())
            .await
            .map_err(|err| Error::Response(format!("{:?}", err)))?
    );
    let parsed = TxResponse::from(client.receive_response()?);
    println!(
        "Transaction applied with result: {}",
        serde_json::to_string_pretty(&parsed).unwrap()
    );
    client.unsubscribe()?;
    client.close();
    Ok(parsed)
}

#[derive(Debug, Serialize)]
pub struct TxResponse {
    info: String,
    height: String,
    hash: String,
    code: String,
    gas_used: String,
    initialized_accounts: Vec<Address>,
}

impl From<serde_json::Value> for TxResponse {
    fn from(json: serde_json::Value) -> Self {
        let mut selector = jsonpath::selector(&json);
        let info = selector("$.events.['applied.info'][0]").unwrap();
        let height = selector("$.events.['applied.height'][0]").unwrap();
        let hash = selector("$.events.['applied.hash'][0]").unwrap();
        let code = selector("$.events.['applied.code'][0]").unwrap();
        let gas_used = selector("$.events.['applied.gas_used'][0]").unwrap();
        let initialized_accounts =
            selector("$.events.['applied.initialized_accounts'][0]");
        let initialized_accounts = match initialized_accounts {
            Ok(values) if !values.is_empty() => {
                // In a response, the initialized accounts are encoded as e.g.:
                // ```
                // "applied.initialized_accounts": Array([
                //   String(
                //     "[\"a1qq5qqqqq8qerqv3sxyuyz3zzxgcyxvecgerry333xce5z3fkg4pnj3zxgfqnzd69gsu5gwzr9wpjpe\"]",
                //   ),
                // ]),
                // ...
                // So we need to decode the inner string first ...
                let raw: String =
                    serde_json::from_value(values[0].clone()).unwrap();
                // ... and then decode the vec from the array inside the string
                serde_json::from_str(&raw).unwrap()
            }
            _ => vec![],
        };
        TxResponse {
            info: serde_json::from_value(info[0].clone()).unwrap(),
            height: serde_json::from_value(height[0].clone()).unwrap(),
            hash: serde_json::from_value(hash[0].clone()).unwrap(),
            code: serde_json::from_value(code[0].clone()).unwrap(),
            gas_used: serde_json::from_value(gas_used[0].clone()).unwrap(),
            initialized_accounts,
        }
    }
}
