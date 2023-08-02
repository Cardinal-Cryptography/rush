use aleph_bft_types::{SpawnHandle, TaskHandle};
use codec::{Decode, Encode};
use futures::{channel::oneshot, Future};

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, Decode, Encode)]
pub struct Spawner;

impl SpawnHandle for Spawner {
    fn spawn(&self, _name: &str, task: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(task);
    }

    fn spawn_essential(
        &self,
        _: &str,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> TaskHandle {
        let handle = tokio::spawn(task);
        Box::pin(async move { handle.await.map_err(|_| ()) })
    }
}

impl Spawner {
    pub fn new() -> Self {
        Spawner {}
    }
}
