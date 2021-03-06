// Copyright 2016 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Grin server implementation, glues the different parts of the system (mostly
//! the peer-to-peer server, the blockchain and the transaction pool) and acts
//! as a facade.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;

use futures::Future;
use tokio_core::reactor;

use adapters::{NetToChainAdapter, ChainToNetAdapter};
use chain;
use chain::ChainStore;
use core;
use miner;
use p2p;

/// Errors than can be reported by a server implementation, mostly wraps
/// underlying components errors.
#[derive(Debug)]
pub enum Error {
	/// Error when trying to add a block to the chain
	ChainErr(chain::pipe::Error),
	/// Peer connection error
	PeerErr(core::ser::Error),
	/// Data store error
	StoreErr(chain::types::Error),
}

/// Full server configuration, aggregating configurations required for the
/// different components.
#[derive(Debug, Clone)]
pub struct ServerConfig {
	/// Directory under which the rocksdb stores will be created
	pub db_root: String,
	/// Allows overriding the default cuckoo cycle size
	pub cuckoo_size: u8,
	/// Configuration for the peer-to-peer server
	pub p2p_config: p2p::P2PConfig,
}

impl Default for ServerConfig {
	fn default() -> ServerConfig {
		ServerConfig {
			db_root: ".grin".to_string(),
			cuckoo_size: 0,
			p2p_config: p2p::P2PConfig::default(),
		}
	}
}

/// Grin server holding internal structures.
pub struct Server {
	config: ServerConfig,
	evt_handle: reactor::Handle,
	/// handle to our network server
	p2p: Arc<p2p::Server>,
	/// the reference copy of the current chain state
	chain_head: Arc<Mutex<chain::Tip>>,
	/// data store access
	chain_store: Arc<chain::ChainStore>,
	/// chain adapter to net, required for miner and anything that submits
	/// blocks
	chain_adapter: Arc<ChainToNetAdapter>,
}

impl Server {
	/// Instantiates and starts a new server.
	pub fn start(config: ServerConfig) -> Result<Server, Error> {
		let (chain_store, head) = try!(store_head(&config));
		let shared_head = Arc::new(Mutex::new(head));

		let chain_adapter = Arc::new(ChainToNetAdapter::new());
		let net_adapter = Arc::new(NetToChainAdapter::new(shared_head.clone(),
		                                                  chain_store.clone(),
		                                                  chain_adapter.clone()));
		let server = Arc::new(p2p::Server::new(config.p2p_config, net_adapter));
		chain_adapter.init(server.clone());

		let mut evtlp = reactor::Core::new().unwrap();
		let handle = evtlp.handle();
		evtlp.run(server.start(handle.clone())).unwrap();

		warn!("Grin server started.");
		Ok(Server {
			config: config,
			evt_handle: handle.clone(),
			p2p: server,
			chain_head: shared_head,
			chain_store: chain_store,
			chain_adapter: chain_adapter,
		})
	}

	/// Instantiates a new server associated with the provided future reactor.
	pub fn future(config: ServerConfig, evt_handle: &reactor::Handle) -> Result<Server, Error> {
		let (chain_store, head) = try!(store_head(&config));
		let shared_head = Arc::new(Mutex::new(head));

		let chain_adapter = Arc::new(ChainToNetAdapter::new());
		let net_adapter = Arc::new(NetToChainAdapter::new(shared_head.clone(),
		                                                  chain_store.clone(),
		                                                  chain_adapter.clone()));
		let server = Arc::new(p2p::Server::new(config.p2p_config, net_adapter));
		chain_adapter.init(server.clone());

		evt_handle.spawn(server.start(evt_handle.clone()).map_err(|_| ()));

		warn!("Grin server started.");
		Ok(Server {
			config: config,
			evt_handle: evt_handle.clone(),
			p2p: server,
			chain_head: shared_head,
			chain_store: chain_store,
			chain_adapter: chain_adapter,
		})
	}

	/// Asks the server to connect to a peer at the provided network address.
	pub fn connect_peer(&self, addr: SocketAddr) -> Result<(), Error> {
		let handle = self.evt_handle.clone();
		handle.spawn(self.p2p.connect_peer(addr, handle.clone()).map_err(|_| ()));
		Ok(())
	}

	/// Start mining for blocks on a separate thread. Relies on a toy miner,
	/// mostly for testing.
	pub fn start_miner(&self) {
		let miner = miner::Miner::new(self.chain_head.clone(),
		                              self.chain_store.clone(),
		                              self.chain_adapter.clone());
		thread::spawn(move || {
			miner.run_loop();
		});
	}

	pub fn head(&self) -> chain::Tip {
		let head = self.chain_head.clone();
		let h = head.lock().unwrap();
		h.clone()
	}
}

// Helper function to create the chain storage and check if it already has a
// genesis block
fn store_head(config: &ServerConfig)
              -> Result<(Arc<chain::store::ChainKVStore>, chain::Tip), Error> {
	let chain_store = try!(chain::store::ChainKVStore::new(config.db_root.clone())
		.map_err(&Error::StoreErr));

	// check if we have a head in store, otherwise the genesis block is it
	let head = match chain_store.head() {
		Ok(tip) => tip,
		Err(chain::types::Error::NotFoundErr) => {
			debug!("No genesis block found, creating and saving one.");
			let mut gen = core::genesis::genesis();
			if config.cuckoo_size > 0 {
				gen.header.cuckoo_len = config.cuckoo_size;
				let diff = gen.header.difficulty.clone();
				core::pow::pow(&mut gen, diff).unwrap();
			}
			try!(chain_store.save_block(&gen).map_err(&Error::StoreErr));
			let tip = chain::types::Tip::new(gen.hash());
			try!(chain_store.save_head(&tip).map_err(&Error::StoreErr));
			tip
		}
		Err(e) => return Err(Error::StoreErr(e)),
	};
	Ok((Arc::new(chain_store), head))
}
