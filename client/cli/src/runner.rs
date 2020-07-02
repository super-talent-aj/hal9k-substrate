// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::CliConfiguration;
use crate::Result;
use crate::Subcommand;
use crate::SubstrateCli;
use chrono::prelude::*;
use futures::pin_mut;
use futures::select;
use futures::{future, future::FutureExt, Future};
use log::info;
use sc_service::{Configuration, TaskType, TaskManager};
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};
use sp_utils::metrics::{TOKIO_THREADS_ALIVE, TOKIO_THREADS_TOTAL};
use std::{fmt::Debug, marker::PhantomData, str::FromStr, sync::Arc};
use sc_client_api::{UsageProvider, BlockBackend, StorageProvider};

#[cfg(target_family = "unix")]
async fn main<F, E>(func: F) -> std::result::Result<(), Box<dyn std::error::Error>>
where
	F: Future<Output = std::result::Result<(), E>> + future::FusedFuture,
	E: 'static + std::error::Error,
{
	use tokio::signal::unix::{signal, SignalKind};

	let mut stream_int = signal(SignalKind::interrupt())?;
	let mut stream_term = signal(SignalKind::terminate())?;

	let t1 = stream_int.recv().fuse();
	let t2 = stream_term.recv().fuse();
	let t3 = func;

	pin_mut!(t1, t2, t3);

	select! {
		_ = t1 => {},
		_ = t2 => {},
		res = t3 => res?,
	}

	Ok(())
}

#[cfg(not(unix))]
async fn main<F, E>(func: F) -> std::result::Result<(), Box<dyn std::error::Error>>
where
	F: Future<Output = std::result::Result<(), E>> + future::FusedFuture,
	E: 'static + std::error::Error,
{
	use tokio::signal::ctrl_c;

	let t1 = ctrl_c().fuse();
	let t2 = func;

	pin_mut!(t1, t2);

	select! {
		_ = t1 => {},
		res = t2 => res?,
	}

	Ok(())
}

/// Build a tokio runtime with all features
pub fn build_runtime() -> std::result::Result<tokio::runtime::Runtime, std::io::Error> {
	tokio::runtime::Builder::new()
		.threaded_scheduler()
		.on_thread_start(|| {
			TOKIO_THREADS_ALIVE.inc();
			TOKIO_THREADS_TOTAL.inc();
		})
		.on_thread_stop(|| {
			TOKIO_THREADS_ALIVE.dec();
		})
		.enable_all()
		.build()
}

fn run_until_exit<FUT, ERR>(
	mut tokio_runtime: tokio::runtime::Runtime, 
	future: FUT, 
	mut task_manager: TaskManager,
) -> Result<()>
where
	FUT: Future<Output = std::result::Result<(), ERR>> + future::Future,
	ERR: 'static + std::error::Error,
{
	let f = future.fuse();
	pin_mut!(f);

	tokio_runtime.block_on(main(f)).map_err(|e| e.to_string())?;

	task_manager.terminate();
	drop(tokio_runtime);

	Ok(())
}

/// A Substrate CLI runtime that can be used to run a node or a command
pub struct Runner<C: SubstrateCli> {
	config: Configuration,
	tokio_runtime: tokio::runtime::Runtime,
	phantom: PhantomData<C>,
}

impl<C: SubstrateCli> Runner<C> {
	/// Create a new runtime with the command provided in argument
	pub fn new<T: CliConfiguration>(cli: &C, command: &T) -> Result<Runner<C>> {
		let tokio_runtime = build_runtime()?;
		let runtime_handle = tokio_runtime.handle().clone();

		let task_executor = move |fut, task_type| {
			match task_type {
				TaskType::Async => { runtime_handle.spawn(fut); }
				TaskType::Blocking => {
					runtime_handle.spawn(async move {
						// `spawn_blocking` is looking for the current runtime, and as such has to
						// be called from within `spawn`.
						tokio::task::spawn_blocking(move || futures::executor::block_on(fut))
					});
				}
			}
		};

		Ok(Runner {
			config: command.create_configuration(cli, task_executor.into())?,
			tokio_runtime,
			phantom: PhantomData,
		})
	}

	/// Log information about the node itself.
	///
	/// # Example:
	///
	/// ```text
	/// 2020-06-03 16:14:21 Substrate Node
	/// 2020-06-03 16:14:21 ✌️  version 2.0.0-rc3-f4940588c-x86_64-linux-gnu
	/// 2020-06-03 16:14:21 ❤️  by Parity Technologies <admin@parity.io>, 2017-2020
	/// 2020-06-03 16:14:21 📋 Chain specification: Flaming Fir
	/// 2020-06-03 16:14:21 🏷  Node name: jolly-rod-7462
	/// 2020-06-03 16:14:21 👤 Role: FULL
	/// 2020-06-03 16:14:21 💾 Database: RocksDb at /tmp/c/chains/flamingfir7/db
	/// 2020-06-03 16:14:21 ⛓  Native runtime: node-251 (substrate-node-1.tx1.au10)
	/// ```
	fn print_node_infos(&self) {
		info!("{}", C::impl_name());
		info!("✌️  version {}", C::impl_version());
		info!(
			"❤️  by {}, {}-{}",
			C::author(),
			C::copyright_start_year(),
			Local::today().year(),
		);
		info!("📋 Chain specification: {}", self.config.chain_spec.name());
		info!("🏷  Node name: {}", self.config.network.node_name);
		info!("👤 Role: {}", self.config.display_role());
		info!("💾 Database: {} at {}",
			self.config.database,
			self.config.database.path().map_or_else(|| "<unknown>".to_owned(), |p| p.display().to_string())
		);
		info!("⛓  Native runtime: {}", C::native_runtime_version(&self.config.chain_spec));
	}

	/// A helper function that runs a future with tokio and stops if the process receives the signal
	/// `SIGTERM` or `SIGINT`.
	pub fn run_subcommand<BU, B, BA, IQ, CL>(self, subcommand: &Subcommand, builder: BU)
		-> Result<()>
	where
		BU: FnOnce(Configuration)
			-> sc_service::error::Result<(Arc<CL>, Arc<BA>, IQ, TaskManager)>,
		B: BlockT + for<'de> serde::Deserialize<'de>,
		BA: sc_client_api::backend::Backend<B> + 'static,
		IQ: sc_service::ImportQueue<B> + 'static,
		<B as BlockT>::Hash: FromStr,
		<<B as BlockT>::Hash as FromStr>::Err: Debug,
		<<<B as BlockT>::Header as HeaderT>::Number as FromStr>::Err: Debug,
		CL: UsageProvider<B> + BlockBackend<B> + StorageProvider<B, BA> + Send + Sync +
		'static,
	{
		let chain_spec = self.config.chain_spec.cloned_box();
		let network_config = self.config.network.clone();
		let db_config = self.config.database.clone();

		match subcommand {
			Subcommand::BuildSpec(cmd) => cmd.run(chain_spec, network_config),
			Subcommand::ExportBlocks(cmd) => {
				let (client, _, _, task_manager) = builder(self.config)?;
				run_until_exit(self.tokio_runtime, cmd.run(client, db_config), task_manager)
			}
			Subcommand::ImportBlocks(cmd) => {
				let (client, _, import_queue, task_manager) = builder(self.config)?;
				run_until_exit(self.tokio_runtime, cmd.run(client, import_queue), task_manager)
			}
			Subcommand::CheckBlock(cmd) => {
				let (client, _, import_queue, task_manager) = builder(self.config)?;
				run_until_exit(self.tokio_runtime, cmd.run(client, import_queue), task_manager)
			}
			Subcommand::Revert(cmd) => {
				let (client, backend, _, task_manager) = builder(self.config)?;
				run_until_exit(self.tokio_runtime, cmd.run(client, backend), task_manager)
			},
			Subcommand::PurgeChain(cmd) => cmd.run(db_config),
			Subcommand::ExportState(cmd) => {
				let (client, _, _, task_manager) = builder(self.config)?;
				run_until_exit(self.tokio_runtime, cmd.run(client, chain_spec), task_manager)
			},
		}
	}

	/// A helper function that runs a node with tokio and stops if the process receives the signal
	/// `SIGTERM` or `SIGINT`.
	pub fn run_node_until_exit(
		mut self,
		initialise: impl FnOnce(Configuration) -> sc_service::error::Result<TaskManager>,
	) -> Result<()> {
		self.print_node_infos();
		let mut task_manager = initialise(self.config)?;
		self.tokio_runtime.block_on(main(task_manager.future().fuse()))
			.map_err(|e| e.to_string())?;
		task_manager.terminate();
		drop(task_manager);
		Ok(())
	}

	/// A helper function that runs a command with the configuration of this node
	pub fn sync_run(self, runner: impl FnOnce(Configuration) -> Result<()>) -> Result<()> {
		runner(self.config)
	}

	/// A helper function that runs a future with tokio and stops if the process receives
	/// the signal SIGTERM or SIGINT
	pub fn async_run<FUT>(
		self, runner: impl FnOnce(Configuration) -> Result<(FUT, TaskManager)>,
	) -> Result<()>
	where
		FUT: Future<Output = Result<()>>,
	{
		let (future, task_manager) = runner(self.config)?;
		run_until_exit(self.tokio_runtime, future, task_manager)
	}

	/// Get an immutable reference to the node Configuration
	pub fn config(&self) -> &Configuration {
		&self.config
	}

	/// Get a mutable reference to the node Configuration
	pub fn config_mut(&mut self) -> &mut Configuration {
		&mut self.config
	}
}
