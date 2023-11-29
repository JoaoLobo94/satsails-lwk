//! # App
//!
//! Contains the RPC server [`App`] wiring the RPC calls to the respective methods in [`Wollet`] or [`Signer`].
//! The server can be configured via the [`Config`] struct.
//!
//! It also contains the RPC client [`Client`].
//!
//! Both the client and the server share the possible [`Error`]s.
//!
//! All the requests and responses data model are in the [`rpc_model`] crate.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use common::{
    keyorigin_xpub_from_str, multisig_desc, singlesig_desc, InvalidBipVariant,
    InvalidBlindingKeyVariant, InvalidMultisigVariant, InvalidSinglesigVariant, Signer,
};
use jade::mutex_jade::MutexJade;
use signer::{AnySigner, SwSigner};
use tiny_jrpc::{tiny_http, JsonRpcServer, Request, Response};
use wollet::bitcoin::bip32::Fingerprint;
use wollet::elements::hex::FromHex;
use wollet::elements::pset::PartiallySignedTransaction;
use wollet::elements_miniscript::descriptor::{Descriptor, DescriptorType, WshInner};
use wollet::elements_miniscript::miniscript::decode::Terminal;
use wollet::Wollet;

use crate::method::Method;
use crate::state::{AppSigner, State};
use rpc_model::{request, response};

pub use client::Client;
pub use config::Config;
pub use error::Error;
pub use tiny_jrpc::RpcError;

mod client;
mod config;
pub mod consts;
mod error;
pub mod method;
mod state;

pub struct App {
    rpc: Option<JsonRpcServer>,
    config: Config,
}

impl App {
    pub fn new(config: Config) -> Result<App, Error> {
        tracing::info!("Creating new app with config: {:?}", config);

        Ok(App { rpc: None, config })
    }

    pub fn run(&mut self) -> Result<(), Error> {
        if self.rpc.is_some() {
            return Err(error::Error::AlreadyStarted);
        }
        let mut state = State {
            config: self.config.clone(),
            ..Default::default()
        };
        state.insert_policy_asset();
        let state = Arc::new(Mutex::new(state));
        let server = tiny_http::Server::http(self.config.addr)?;

        let rpc = tiny_jrpc::JsonRpcServer::new(
            server,
            tiny_jrpc::Config::default(),
            state,
            method_handler,
        );
        self.rpc = Some(rpc);
        Ok(())
    }

    pub fn stop(&self) -> Result<(), Error> {
        match self.rpc.as_ref() {
            Some(rpc) => {
                rpc.stop();
                Ok(())
            }
            None => Err(error::Error::NotStarted),
        }
    }

    pub fn is_running(&self) -> Result<bool, Error> {
        match self.rpc.as_ref() {
            Some(rpc) => Ok(rpc.is_running()),
            None => Err(error::Error::NotStarted),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.config.addr
    }

    pub fn join_threads(&mut self) -> Result<(), Error> {
        self.rpc
            .take()
            .ok_or(error::Error::NotStarted)?
            .join_threads();
        Ok(())
    }

    pub fn client(&self) -> Result<Client, Error> {
        Client::new(self.config.addr)
    }
}

fn method_handler(
    request: Request,
    state: Arc<Mutex<State>>,
) -> Result<Response, tiny_jrpc::Error> {
    Ok(inner_method_handler(request, state)?)
}

fn inner_method_handler(request: Request, state: Arc<Mutex<State>>) -> Result<Response, Error> {
    tracing::debug!(
        "method: {} params: {:?} ",
        request.method.as_str(),
        request.params
    );
    let method: Method = match request.method.as_str().parse() {
        Ok(method) => method,
        Err(e) => return Ok(Response::unimplemented(request.id, e.to_string())),
    };
    let response = match method {
        Method::Schema => {
            let r: request::Schema = serde_json::from_value(request.params.unwrap_or_default())?;
            let method: Method = r.method.parse()?;
            Response::result(request.id, method.schema(r.direction)?)
        }
        Method::GenerateSigner => {
            let (_signer, mnemonic) = SwSigner::random()?;
            Response::result(
                request.id,
                serde_json::to_value(response::GenerateSigner {
                    mnemonic: mnemonic.to_string(),
                })?,
            )
        }
        Method::Version => Response::result(
            request.id,
            serde_json::to_value(response::Version {
                version: consts::APP_VERSION.into(),
            })?,
        ),
        Method::LoadWallet => {
            let r: request::LoadWallet =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            // TODO recognize different name same descriptor?
            let wollet = Wollet::new(
                s.config.network.clone(),
                &s.config.electrum_url,
                s.config.tls,
                s.config.validate_domain,
                &s.config.datadir,
                &r.descriptor,
            )?;
            s.wollets.insert(&r.name, wollet)?;
            Response::result(
                request.id,
                serde_json::to_value(response::Wallet {
                    descriptor: r.descriptor,
                    name: r.name,
                })?,
            )
        }
        Method::UnloadWallet => {
            let r: request::UnloadWallet =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            let removed = s.wollets.remove(&r.name)?;
            Response::result(
                request.id,
                serde_json::to_value(response::UnloadWallet {
                    unloaded: response::Wallet {
                        name: r.name,
                        descriptor: removed.descriptor().to_string(),
                    },
                })?,
            )
        }
        Method::ListWallets => {
            let s = state.lock().unwrap();
            let wallets = s
                .wollets
                .iter()
                .map(|(name, wollet)| response::Wallet {
                    descriptor: wollet.descriptor().to_string(),
                    name: name.clone(),
                })
                .collect();
            let r = response::ListWallets { wallets };
            Response::result(request.id, serde_json::to_value(r)?)
        }
        Method::LoadSigner => {
            let r: request::LoadSigner =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();

            let signer = match r.kind.as_str() {
                "software" => {
                    if r.mnemonic.is_none() {
                        return Err(Error::Generic(
                            "Mnemonic must be set for software signer".to_string(),
                        ));
                    }
                    let mnemonic = r.mnemonic.unwrap();
                    AppSigner::AvailableSigner(AnySigner::Software(SwSigner::new(&mnemonic)?))
                }
                "serial" => {
                    let network = s.config.jade_network();
                    let jade = MutexJade::from_serial(network)?;
                    jade.unlock().unwrap();
                    AppSigner::AvailableSigner(AnySigner::Jade(jade))
                }
                "external" => {
                    if r.fingerprint.is_none() {
                        return Err(Error::Generic(
                            "Fingerprint must be set for external signer".to_string(),
                        ));
                    }
                    let fingerprint = Fingerprint::from_str(&r.fingerprint.unwrap())
                        .map_err(|e| Error::Generic(e.to_string()))?;
                    AppSigner::ExternalSigner(fingerprint)
                }
                _ => {
                    return Err(Error::Generic("Invalid signer kind".to_string()));
                }
            };

            let resp: response::Signer = signer_response_from(&r.name, &signer)?;
            s.signers.insert(&r.name, signer)?;
            Response::result(request.id, serde_json::to_value(resp)?)
        }
        Method::UnloadSigner => {
            let r: request::UnloadSigner =
                serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            let removed = s.signers.remove(&r.name)?;
            let signer: response::Signer = signer_response_from(&r.name, &removed)?;
            Response::result(
                request.id,
                serde_json::to_value(response::UnloadSigner { unloaded: signer })?,
            )
        }
        Method::ListSigners => {
            let s = state.lock().unwrap();
            let signers: Result<Vec<_>, _> = s
                .signers
                .iter()
                .map(|(name, signer)| signer_response_from(name, signer))
                .collect();
            let r = response::ListSigners { signers: signers? };
            Response::result(request.id, serde_json::to_value(r)?)
        }
        Method::Address => {
            let r: request::Address = serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;
            wollet.sync_txs()?; // To update the last unused index
            let addr = wollet.address(r.index)?;
            Response::result(
                request.id,
                serde_json::to_value(response::Address {
                    address: addr.address().clone(),
                    index: addr.index(),
                })?,
            )
        }
        Method::Balance => {
            let r: request::Balance = serde_json::from_value(request.params.unwrap_or_default())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;
            wollet.sync_txs()?;
            let balance = wollet.balance()?;
            Response::result(
                request.id,
                serde_json::to_value(response::Balance { balance })?,
            )
        }
        Method::SendMany => {
            let r: request::Send = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;
            wollet.sync_txs()?;
            let tx = wollet.send_many(
                r.addressees
                    .into_iter()
                    .map(unvalidated_addressee)
                    .collect(),
                r.fee_rate,
            )?;
            Response::result(
                request.id,
                serde_json::to_value(response::Pset {
                    pset: tx.to_string(),
                })?,
            )
        }
        Method::SinglesigDescriptor => {
            let r: request::SinglesigDescriptor = serde_json::from_value(request.params.unwrap())?;
            let s = state.lock().unwrap();

            let signer = s.signers.get_available(&r.name)?;

            let script_variant = r
                .singlesig_kind
                .parse()
                .map_err(|e: InvalidSinglesigVariant| e.to_string())?;

            let blinding_variant = r
                .descriptor_blinding_key
                .parse()
                .map_err(|e: InvalidBlindingKeyVariant| e.to_string())?;

            let descriptor = singlesig_desc(signer, script_variant, blinding_variant).unwrap();
            Response::result(
                request.id,
                serde_json::to_value(response::SinglesigDescriptor { descriptor })?,
            )
        }
        Method::MultisigDescriptor => {
            let r: request::MultisigDescriptor = serde_json::from_value(request.params.unwrap())?;

            let multisig_variant = r
                .multisig_kind
                .parse()
                .map_err(|e: InvalidMultisigVariant| e.to_string())?;

            let blinding_variant = r
                .descriptor_blinding_key
                .parse()
                .map_err(|e: InvalidBlindingKeyVariant| e.to_string())?;

            let mut keyorigin_xpubs = vec![];
            for keyorigin_xpub in r.keyorigin_xpubs {
                keyorigin_xpubs.push(
                    keyorigin_xpub_from_str(&keyorigin_xpub)
                        .map_err(|e| Error::Generic(e.to_string()))?,
                );
            }

            let descriptor = multisig_desc(
                r.threshold,
                keyorigin_xpubs,
                multisig_variant,
                blinding_variant,
            )?;
            Response::result(
                request.id,
                serde_json::to_value(response::MultisigDescriptor { descriptor })?,
            )
        }
        Method::Xpub => {
            let r: request::Xpub = serde_json::from_value(request.params.unwrap())?;
            let s = state.lock().unwrap();

            let signer = s.signers.get_available(&r.name)?;

            let bip = r
                .xpub_kind
                .parse()
                .map_err(|e: InvalidBipVariant| e.to_string())?;

            let keyorigin_xpub = signer.keyorigin_xpub(bip)?;
            Response::result(
                request.id,
                serde_json::to_value(response::Xpub { keyorigin_xpub })?,
            )
        }
        Method::Sign => {
            let r: request::Sign = serde_json::from_value(request.params.unwrap())?;
            let s = state.lock().unwrap();

            let signer = s.signers.get_available(&r.name)?;

            let mut pset =
                PartiallySignedTransaction::from_str(&r.pset).map_err(|e| e.to_string())?;

            signer.sign(&mut pset)?;

            // TODO we may want to return other details such as if signatures have been added

            Response::result(
                request.id,
                serde_json::to_value(response::Pset {
                    pset: pset.to_string(),
                })?,
            )
        }
        Method::Broadcast => {
            let r: request::Broadcast = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;
            let mut pset =
                PartiallySignedTransaction::from_str(&r.pset).map_err(|e| e.to_string())?;
            let tx = wollet.finalize(&mut pset)?;

            if !r.dry_run {
                wollet.broadcast(&tx)?;
            }

            Response::result(
                request.id,
                serde_json::to_value(response::Broadcast { txid: tx.txid() })?,
            )
        }
        Method::WalletDetails => {
            let r: request::WalletDetails = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;

            let type_ = match wollet.descriptor().descriptor.desc_type() {
                DescriptorType::Wpkh => request::WalletType::Wpkh,
                DescriptorType::ShWpkh => request::WalletType::ShWpkh,
                _ => match &wollet.descriptor().descriptor {
                    Descriptor::Wsh(wsh) => match wsh.as_inner() {
                        WshInner::Ms(ms) => match &ms.node {
                            Terminal::Multi(threshold, pubkeys) => {
                                request::WalletType::WshMulti(*threshold, pubkeys.len())
                            }
                            _ => request::WalletType::Unknown,
                        },
                        _ => request::WalletType::Unknown,
                    },
                    _ => request::WalletType::Unknown,
                },
            };

            let mut warnings: Vec<String> = vec![];

            let has_unique_fingerprints = {
                let mut hs = HashSet::new();
                wollet.signers().into_iter().all(|f| hs.insert(f))
            };
            if !has_unique_fingerprints {
                warnings.push("wallet has multiple signers with the same fingerprint".into());
            }

            let signers: Vec<_> = wollet
                .signers()
                .iter()
                .map(|fingerprint| {
                    let name = s.signers.name_from_fingerprint(fingerprint, &mut warnings);
                    response::SignerDetails {
                        name,
                        fingerprint: *fingerprint,
                    }
                })
                .collect();

            Response::result(
                request.id,
                serde_json::to_value(response::WalletDetails {
                    type_: type_.to_string(),
                    signers,
                    warnings: warnings.join(", "),
                })?,
            )
        }
        Method::WalletCombine => {
            let r: request::WalletCombine = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;

            let mut psets = vec![];
            for pset in r.pset {
                psets.push(PartiallySignedTransaction::from_str(&pset).map_err(|e| e.to_string())?);
            }
            let pset = wollet.combine(&psets)?;
            Response::result(
                request.id,
                serde_json::to_value(response::WalletCombine {
                    pset: pset.to_string(),
                })?,
            )
        }
        Method::WalletPsetDetails => {
            let r: request::WalletPsetDetails = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;

            let pset = PartiallySignedTransaction::from_str(&r.pset).map_err(|e| e.to_string())?;
            let details = wollet.get_details(&pset)?;
            let mut warnings = vec![];
            let has_signatures_from = details
                .fingerprints_has()
                .iter()
                .map(|f| response::SignerDetails {
                    name: s.signers.name_from_fingerprint(f, &mut warnings),
                    fingerprint: *f,
                })
                .collect();
            let missing_signatures_from = details
                .fingerprints_missing()
                .iter()
                .map(|f| response::SignerDetails {
                    name: s.signers.name_from_fingerprint(f, &mut warnings),
                    fingerprint: *f,
                })
                .collect();
            Response::result(
                request.id,
                serde_json::to_value(response::WalletPsetDetails {
                    has_signatures_from,
                    missing_signatures_from,
                    warnings: warnings.join(", "),
                })?,
            )
        }
        Method::Issue => {
            let r: request::Issue = serde_json::from_value(request.params.unwrap())?;
            let mut s = state.lock().unwrap();
            let wollet = s.wollets.get_mut(&r.name)?;
            wollet.sync_txs()?;
            let tx = wollet.issue_asset(
                r.satoshi_asset,
                r.address_asset.as_deref().unwrap_or(""),
                r.satoshi_token,
                r.address_token.as_deref().unwrap_or(""),
                r.contract.as_deref().unwrap_or(""),
                r.fee_rate,
            )?;
            Response::result(
                request.id,
                serde_json::to_value(response::Pset {
                    pset: tx.to_string(),
                })?,
            )
        }
        Method::Contract => {
            let r: request::Contract = serde_json::from_value(request.params.unwrap())?;
            let c = wollet::Contract {
                entity: wollet::Entity::Domain(r.domain),
                issuer_pubkey: Vec::<u8>::from_hex(&r.issuer_pubkey)?,
                name: r.name,
                precision: r.precision,
                ticker: r.ticker,
                version: r.version,
            };
            c.validate()?; // TODO: validation should be done at Contract creation

            Response::result(request.id, serde_json::to_value(c)?)
        }
        Method::AssetDetails => {
            let r: request::AssetDetails = serde_json::from_value(request.params.unwrap())?;
            let s = state.lock().unwrap();
            let asset_id = wollet::elements::AssetId::from_str(&r.asset_id)
                .map_err(|e| Error::Generic(e.to_string()))?;
            let asset = s.get_asset(&asset_id)?;
            Response::result(
                request.id,
                serde_json::to_value(response::AssetDetails { name: asset.name() })?,
            )
        }
        Method::Stop => {
            return Err(Error::Stop);
        }
    };
    Ok(response)
}

fn unvalidated_addressee(a: request::UnvalidatedAddressee) -> wollet::UnvalidatedAddressee {
    wollet::UnvalidatedAddressee {
        satoshi: a.satoshi,
        address: a.address,
        asset: a.asset,
    }
}

fn signer_response_from(
    name: &str,
    signer: &AppSigner,
) -> Result<response::Signer, signer::SignerError> {
    let (fingerprint, id, xpub) = match signer {
        AppSigner::AvailableSigner(signer) => (
            signer.fingerprint()?,
            Some(signer.identifier()?),
            Some(signer.xpub()?),
        ),
        AppSigner::ExternalSigner(fingerprint) => (*fingerprint, None, None),
    };

    Ok(response::Signer {
        name: name.to_string(),
        id,
        fingerprint,
        xpub,
    })
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn app_random_port() -> App {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let config = Config {
            addr,
            ..Default::default()
        };
        let mut app = App::new(config).unwrap();
        app.run().unwrap();
        app
    }

    #[test]
    fn version() {
        let mut app = app_random_port();
        let addr = app.addr();
        let url = addr.to_string();
        dbg!(&url);

        let client = jsonrpc::Client::simple_http(&url, None, None).unwrap();
        let request = client.build_request("version", None);
        let response = client.send_request(request).unwrap();

        let result = response.result.unwrap().to_string();
        let actual: response::Version = serde_json::from_str(&result).unwrap();
        assert_eq!(actual.version, consts::APP_VERSION);

        app.stop().unwrap();
        app.join_threads().unwrap();
    }
}
