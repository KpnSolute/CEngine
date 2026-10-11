//! A small blocking connection pool for the typed store.
//!
//! Connections are made on demand up to `max`, handed out one per operation or transaction,
//! and returned on drop. A connection that closed while in use is discarded, so the next
//! caller gets a fresh one. Callers wait for a free connection for at most `wait`, then get a
//! backend error instead of hanging. How a connection is made (plain, or later with TLS) is a
//! closure, so the pool itself does not know about either.

use ce_store::StoreError;
use postgres::Client;
use std::{
    ops::{Deref, DerefMut},
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

/// Pool limits. The defaults suit one server process.
#[derive(Clone, Copy, Debug)]
pub struct PoolConfig {
    /// Most connections open at once (at least 1).
    pub max: usize,
    /// Longest a caller waits for a free connection.
    pub wait: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            max: 8,
            wait: Duration::from_secs(30),
        }
    }
}

type Connect = Box<dyn Fn() -> Result<Client, StoreError> + Send + Sync>;

struct State {
    idle: Vec<Client>,
    open: usize,
}

pub(crate) struct Pool {
    connect: Connect,
    cfg: PoolConfig,
    state: Mutex<State>,
    freed: Condvar,
}

impl Pool {
    pub(crate) fn new(connect: Connect, cfg: PoolConfig) -> Pool {
        Pool {
            connect,
            cfg: PoolConfig {
                max: cfg.max.max(1),
                ..cfg
            },
            state: Mutex::new(State {
                idle: Vec::new(),
                open: 0,
            }),
            freed: Condvar::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(&self) -> Result<Conn<'_>, StoreError> {
        let deadline = Instant::now() + self.cfg.wait;
        let mut st = self.state();
        loop {
            while let Some(c) = st.idle.pop() {
                if c.is_closed() {
                    st.open -= 1;
                    continue;
                }
                return Ok(Conn {
                    pool: self,
                    client: Some(c),
                });
            }
            if st.open < self.cfg.max {
                st.open += 1;
                drop(st);
                return match (self.connect)() {
                    Ok(c) => Ok(Conn {
                        pool: self,
                        client: Some(c),
                    }),
                    Err(e) => {
                        self.state().open -= 1;
                        self.freed.notify_one();
                        Err(e)
                    }
                };
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(StoreError::Backend(
                    "no database connection became free in time".to_string(),
                ));
            }
            st = self
                .freed
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// A checked-out connection; goes back to the pool when dropped.
pub(crate) struct Conn<'a> {
    pool: &'a Pool,
    client: Option<Client>,
}

impl Deref for Conn<'_> {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.client.as_ref().expect("connection present until drop")
    }
}

impl DerefMut for Conn<'_> {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("connection present until drop")
    }
}

impl Drop for Conn<'_> {
    fn drop(&mut self) {
        let Some(c) = self.client.take() else { return };
        let mut st = self.pool.state();
        if c.is_closed() {
            st.open -= 1;
        } else {
            st.idle.push(c);
        }
        drop(st);
        self.pool.freed.notify_one();
    }
}
