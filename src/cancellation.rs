use std::sync::Arc;

use tokio::sync::Notify;

pub struct CancellationToken {
    notify: Arc<Notify>,
    finish: bool,
}

impl CancellationToken {
    pub fn new(notify: Arc<Notify>) -> CancellationToken {
        CancellationToken {
            notify,
            finish: false,
        }
    }

    pub fn finish(&mut self) {
        self.finish = true;
    }
}

impl Drop for CancellationToken {
    fn drop(&mut self) {
        if !self.finish {
            self.notify.notify_one();
        }
    }
}
