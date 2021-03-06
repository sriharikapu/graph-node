use failure::*;
use futures::sync::mpsc::{channel, Sender};
use futures::sync::oneshot;
use std::thread;

use graph::components::ethereum::*;
use graph::components::store::Store;
use graph::data::subgraph::DataSource;
use graph::ethabi::LogParam;
use graph::ethabi::RawLog;
use graph::prelude::{
    MappingABI, RuntimeHost as RuntimeHostTrait, RuntimeHostBuilder as RuntimeHostBuilderTrait, *,
};
use graph::util;
use graph::web3::types::{Log, Transaction};

use super::EventHandlerContext;
use module::{WasmiModule, WasmiModuleConfig};

#[derive(Clone)]
pub struct RuntimeHostConfig {
    subgraph_manifest: SubgraphManifest,
    data_source: DataSource,
}

pub struct RuntimeHostBuilder<T, L, S> {
    ethereum_adapter: Arc<T>,
    link_resolver: Arc<L>,
    store: Arc<S>,
}

impl<T, L, S> Clone for RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    fn clone(&self) -> Self {
        RuntimeHostBuilder {
            ethereum_adapter: self.ethereum_adapter.clone(),
            link_resolver: self.link_resolver.clone(),
            store: self.store.clone(),
        }
    }
}

impl<T, L, S> RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    pub fn new(ethereum_adapter: Arc<T>, link_resolver: Arc<L>, store: Arc<S>) -> Self {
        RuntimeHostBuilder {
            ethereum_adapter,
            link_resolver,
            store,
        }
    }
}

impl<T, L, S> RuntimeHostBuilderTrait for RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    type Host = RuntimeHost;

    fn build(
        &self,
        logger: &Logger,
        subgraph_manifest: SubgraphManifest,
        data_source: DataSource,
    ) -> Self::Host {
        RuntimeHost::new(
            logger,
            self.ethereum_adapter.clone(),
            self.link_resolver.clone(),
            self.store.clone(),
            RuntimeHostConfig {
                subgraph_manifest,
                data_source,
            },
        )
    }
}

type HandleEventResponse = Result<Vec<EntityOperation>, Error>;

struct HandleEventRequest {
    handler: MappingEventHandler,
    logger: Logger,
    block: Arc<EthereumBlock>,
    transaction: Arc<Transaction>,
    log: Arc<Log>,
    params: Vec<LogParam>,
    entity_operations: Vec<EntityOperation>,
    result_sender: oneshot::Sender<HandleEventResponse>,
}

pub struct RuntimeHost {
    config: RuntimeHostConfig,
    handle_event_sender: Sender<HandleEventRequest>,
    _guard: oneshot::Sender<()>,
}

impl RuntimeHost {
    pub fn new<T, L, S>(
        logger: &Logger,
        ethereum_adapter: Arc<T>,
        link_resolver: Arc<L>,
        store: Arc<S>,
        config: RuntimeHostConfig,
    ) -> Self
    where
        T: EthereumAdapter,
        L: LinkResolver,
        S: Store,
    {
        let logger = logger.new(o!(
            "component" => "RuntimeHost",
            "data_source" => config.data_source.name.clone(),
        ));

        // Create channel for canceling the module
        let (cancel_sender, cancel_receiver) = oneshot::channel();

        // Create channel for event handling requests
        let (handle_event_sender, handle_event_receiver) = channel(100);

        // wasmi modules are not `Send` therefore they cannot be scheduled by
        // the regular tokio executor, so we create a dedicated thread.
        //
        // This thread can spawn tasks on the runtime by sending them to
        // `task_receiver`.
        let (task_sender, task_receiver) = channel(100);
        tokio::spawn(task_receiver.for_each(tokio::spawn));

        // Start the WASMI module runtime
        let module_logger = logger.clone();
        let module_config = config.clone();
        thread::spawn(move || {
            debug!(module_logger, "Start WASM runtime");

            // Load the mapping of the data source as a WASM module
            let wasmi_config = WasmiModuleConfig {
                subgraph: module_config.subgraph_manifest.clone(),
                data_source: module_config.data_source.clone(),
                ethereum_adapter: ethereum_adapter.clone(),
                link_resolver: link_resolver.clone(),
                store: store.clone(),
            };

            // Start the mapping as a WASM module
            let mut module = WasmiModule::new(&module_logger, wasmi_config, task_sender);

            // Pass incoming events to the WASM module and send entity changes back;
            // stop when cancelled from the outside
            handle_event_receiver
                .map(Some)
                .select(cancel_receiver.into_stream().map(|_| None).map_err(|_| ()))
                .for_each(move |request: Option<HandleEventRequest>| {
                    if let Some(request) = request {
                        let HandleEventRequest {
                            handler,
                            logger,
                            block,
                            transaction,
                            log,
                            params,
                            entity_operations,
                            result_sender,
                        } = request;

                        let ctx = EventHandlerContext {
                            logger,
                            block,
                            transaction,
                            entity_operations,
                        };

                        let result = module.handle_ethereum_event(
                            ctx,
                            handler.handler.as_str(),
                            log,
                            params,
                        );
                        future::result(result_sender.send(result).map_err(|_| ()))
                    } else {
                        future::err(())
                    }
                }).wait()
                .ok();
        });

        RuntimeHost {
            config,
            handle_event_sender,
            _guard: cancel_sender,
        }
    }

    fn matches_log_address(&self, log: &Log) -> bool {
        self.config.data_source.source.address == log.address
    }

    fn matches_log_signature(&self, log: &Log) -> bool {
        if log.topics.is_empty() {
            return false;
        }
        let signature = log.topics[0];

        self.config
            .data_source
            .mapping
            .event_handlers
            .iter()
            .any(|event_handler| {
                signature == util::ethereum::string_to_h256(event_handler.event.as_str())
            })
    }

    fn source_contract(&self) -> Result<&MappingABI, Error> {
        self.config
            .data_source
            .mapping
            .abis
            .iter()
            .find(|abi| abi.name == self.config.data_source.source.abi)
            .ok_or_else(|| {
                format_err!(
                    "No ABI entry found for the main contract of data source \"{}\": {}",
                    self.config.data_source.name,
                    self.config.data_source.source.abi,
                )
            })
    }

    fn event_handler_for_log(&self, log: &Arc<Log>) -> Result<&MappingEventHandler, Error> {
        // Get signature from the log
        if log.topics.is_empty() {
            return Err(format_err!("Ethereum event has no topics"));
        }
        let signature = log.topics[0];

        self.config
            .data_source
            .mapping
            .event_handlers
            .iter()
            .find(|handler| signature == util::ethereum::string_to_h256(handler.event.as_str()))
            .ok_or_else(|| {
                format_err!(
                    "No event handler found for event in data source \"{}\"",
                    self.config.data_source.name,
                )
            })
    }
}

impl RuntimeHostTrait for RuntimeHost {
    fn subgraph_manifest(&self) -> &SubgraphManifest {
        &self.config.subgraph_manifest
    }

    fn matches_log(&self, log: &Log) -> bool {
        self.matches_log_address(log) && self.matches_log_signature(log)
    }

    fn process_log(
        &self,
        logger: &Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        log: Arc<Log>,
        entity_operations: Vec<EntityOperation>,
    ) -> Box<Future<Item = Vec<EntityOperation>, Error = Error> + Send> {
        // Identify the contract that the log belongs to
        let abi = match self.source_contract() {
            Ok(abi) => abi,
            Err(e) => return Box::new(future::err(e)),
        };

        // Identify event handler for this log
        let event_handler = match self.event_handler_for_log(&log) {
            Ok(handler) => handler,
            Err(e) => return Box::new(future::err(e)),
        };

        // Identify the event ABI in the contract
        let event_abi = match util::ethereum::contract_event_with_signature(
            &abi.contract,
            event_handler.event.as_str(),
        ) {
            Some(event_abi) => event_abi,
            None => {
                return Box::new(future::err(format_err!(
                    "Event with the signature \"{}\" not found in \
                     contract \"{}\" of data source \"{}\"",
                    event_handler.event,
                    abi.name,
                    self.config.data_source.name
                )))
            }
        };

        // Parse the log into an event
        let params = match event_abi.parse_log(RawLog {
            topics: log.topics.clone(),
            data: log.data.clone().0,
        }) {
            Ok(log) => log.params,
            Err(e) => {
                return Box::new(future::err(format_err!(
                    "Failed to parse parameters of event: {}: {}",
                    event_handler.event,
                    e
                )))
            }
        };

        debug!(
            logger, "Process Ethereum event";
            "signature" => &event_handler.event,
            "handler" => &event_handler.handler
        );

        // Call the event handler and asynchronously wait for the result
        let (result_sender, result_receiver) = oneshot::channel();

        let before_event_signature = event_handler.event.clone();
        let event_signature = event_handler.event.clone();

        Box::new(
            self.handle_event_sender
                .clone()
                .send(HandleEventRequest {
                    handler: event_handler.clone(),
                    logger: logger.clone(),
                    block: block.clone(),
                    transaction: transaction.clone(),
                    log: log.clone(),
                    params,
                    entity_operations,
                    result_sender,
                }).map_err(move |_| {
                    format_err!(
                        "Mapping terminated before passing in Ethereum event: {}",
                        before_event_signature
                    )
                }).and_then(|_| {
                    result_receiver.map_err(move |_| {
                        format_err!(
                            "Mapping terminated before finishing to handle \
                             Ethereum event: {}",
                            event_signature,
                        )
                    })
                }).and_then(|result| result),
        )
    }
}
