use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use tokio::time::sleep;

use crate::{
    backend::Backend,
    task::{Error, Task},
    typemap::TypeMap,
};

pub struct Broker<B> {
    backend: Arc<B>,
    poll_interval: u64,
    data: Arc<TypeMap>,
    active_tasks:
        Arc<tokio::sync::RwLock<HashMap<String, tokio::task::JoinHandle<Result<(), Error>>>>>,
    handlers: Arc<HashMap<String, Arc<dyn Task + Send + Sync>>>,
}

pub struct NewWorkRequest {
    pub action: String,
    pub data: serde_json::Value,
    pub not_before: Option<DateTime<Utc>>,
}

impl<B: Backend + Send + Sync + 'static> Broker<B> {
    pub fn new(
        backend: Arc<B>,
        poll_interval: u64,
        data: Arc<TypeMap>,
        handlers: Arc<HashMap<String, Arc<dyn Task + Send + Sync>>>,
    ) -> Self {
        Self {
            backend,
            poll_interval,
            data,
            active_tasks: Default::default(),
            handlers,
        }
    }

    pub async fn add_work(&self, work: NewWorkRequest) -> Result<(), B::Error> {
        self.backend.add_work_request(work).await
    }

    async fn _start(
        backend: Arc<B>,
        data: Arc<TypeMap>,
        active_tasks_lock:
            Arc<tokio::sync::RwLock<HashMap<String, tokio::task::JoinHandle<Result<(), Error>>>>>,
        handlers: Arc<HashMap<String, Arc<dyn Task + Send + Sync>>>,
        poll_interval: u64,
    ) {
        tracing::info!("starting broker worker loop");
        loop {
            tracing::debug!("Polling ids");
            let new_ids = match backend.poll().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error=%e, "Failed to poll broker backend");
                    sleep(Duration::from_secs(poll_interval)).await;
                    continue;
                }
            };

            tracing::debug!(count=new_ids.len(), "New ids");
            for id in new_ids {
                let active_tasks = active_tasks_lock.read().await;
                if active_tasks.contains_key(&id) {
                    continue;
                }

                let work_request = match backend.work_request_with_id(&id).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error=%e, id=id, "No work request found for id");
                        continue;
                    }
                };

                let Some(handler) = handlers.get(&work_request.action).map(Arc::clone) else {
                    tracing::error!(action=work_request.action, "No handler found");
                    continue;
                };

                drop(active_tasks);
                let (tx, rx) = tokio::sync::oneshot::channel();

                let handle = {
                    let active_tasks_lock = Arc::clone(&active_tasks_lock);
                    let data = Arc::clone(&data);
                    let id = id.clone();

                    tokio::task::spawn(async move {
                        let _mm = rx.await.unwrap();

                        let result = handler.run(&*data, work_request).await;
                        active_tasks_lock.write().await.remove(&id);
                        result
                    })
                };

                active_tasks_lock.write().await.insert(id.clone(), handle);
                tx.send(()).unwrap();
            }

            sleep(Duration::from_secs(poll_interval)).await;
        }
    }

    pub fn start_workers(&self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(Self::_start(
            self.backend.clone(), 
            self.data.clone(), 
            self.active_tasks.clone(), 
            self.handlers.clone(),
            self.poll_interval,
        ))
    }
}
