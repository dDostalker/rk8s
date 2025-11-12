use std::{collections::HashMap, str::FromStr, sync::Arc};

use anyhow::{Result, anyhow};
use hickory_proto::{
    op::ResponseCode,
    rr::{LowerName, RData, RecordSet, RecordType, rdata::A},
};
use hickory_server::{
    authority::{
        Authority, LookupControlFlow, LookupOptions, LookupRecords, MessageRequest, ZoneType,
    },
    server::RequestInfo,
};
use tracing::debug;

pub struct LocalAuthority {
    pub origin: LowerName,
    pub store: Arc<dyn RecordStore + Send + Sync>,
}

#[async_trait::async_trait]
impl Authority for LocalAuthority {
    type Lookup = LookupRecords;

    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    fn is_axfr_allowed(&self) -> bool {
        false
    }

    async fn update(&self, _: &MessageRequest) -> Result<bool, ResponseCode> {
        Ok(false)
    }

    async fn search(
        &self,
        request: RequestInfo<'_>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<Self::Lookup> {
        debug!("DNS search for: {:?}", request.query.name());
        <LocalAuthority as Authority>::lookup(
            self,
            request.query.name(),
            request.query.query_type(),
            lookup_options,
        )
        .await
    }

    async fn get_nsec_records(
        &self,
        name: &LowerName,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<Self::Lookup> {
        LookupControlFlow::Continue(Ok(LookupRecords::Records {
            lookup_options,
            records: Arc::new(RecordSet::new(name.clone().into(), RecordType::NSEC, 0)),
        }))
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<Self::Lookup> {
        return LookupControlFlow::Continue(Ok(LookupRecords::Empty));
    }
}

impl LocalAuthority {
    pub fn from_mem(origin: &str) -> Self {
        let map: HashMap<LowerName, RData> = HashMap::new();
        let mem_store = MemStore(map);
        Self {
            origin: LowerName::from_str(origin).unwrap(),
            store: Arc::new(mem_store),
        }
    }

    pub async fn start_watch(&self) {}

    pub async fn start(
        origin: &str,
        store: Arc<dyn RecordStore + Send + Sync>,
    ) -> Result<Arc<Self>> {
        // TODO: start daemon

        Ok(Arc::new(Self {
            origin: LowerName::from_str(origin).unwrap(),
            store,
        }))
    }
}

#[async_trait::async_trait]
pub trait RecordStore {
    async fn add(&mut self, name: LowerName, record: RData) -> Result<()>;
    async fn del(&mut self, name: LowerName) -> Result<()>;
    async fn get(&self, name: LowerName) -> Result<RData>;
}

pub struct MemStore(HashMap<LowerName, RData>);

#[async_trait::async_trait]
impl RecordStore for MemStore {
    async fn add(&mut self, name: LowerName, record: RData) -> Result<()> {
        self.0.insert(name.clone(), record.clone());
        Ok(())
    }
    async fn del(&mut self, name: LowerName) -> Result<()> {
        self.0.remove(&name);
        Ok(())
    }
    async fn get(&self, name: LowerName) -> Result<RData> {
        self.0
            .get(&name)
            .ok_or_else(|| anyhow!("record not found"))
            .cloned()
    }
}

impl MemStore {
    pub fn new() -> Self {
        let map: HashMap<LowerName, RData> = HashMap::new();
        MemStore(map)
    }
}
