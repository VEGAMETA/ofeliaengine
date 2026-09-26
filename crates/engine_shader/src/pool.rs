//! A pool of Slang sessions shared by compiling threads.
//!
//! A session is single-threaded, takes ~100–250 ms to create and holds ~150 MB (Slang's
//! core module; measured in ADR 0009). The pool creates sessions lazily, only as many as
//! compile concurrently, and keeps them for the next build or hot reload.

use crate::ShaderError;
use crate::compiler::Compiler;
use crate::slang::SlangLibrary;
use std::sync::{Arc, Mutex, PoisonError};

/// Idle compilers, created on demand.
#[derive(Debug)]
pub struct CompilerPool {
    library: Arc<SlangLibrary>,
    idle: Mutex<Vec<Compiler>>,
}

impl CompilerPool {
    /// An empty pool; no session is created until the first compile.
    pub fn new(library: Arc<SlangLibrary>) -> Self {
        Self {
            library,
            idle: Mutex::new(Vec::new()),
        }
    }

    /// The Slang library.
    pub fn library(&self) -> &Arc<SlangLibrary> {
        &self.library
    }

    /// Runs `f` with an idle compiler, creating one if none is idle.
    ///
    /// The lock is held only to take and return the compiler, never while compiling.
    pub fn with<R>(&self, f: impl FnOnce(&mut Compiler) -> R) -> Result<R, ShaderError> {
        let idle = self.lock().pop();
        let mut compiler = match idle {
            Some(compiler) => compiler,
            None => Compiler::new(Arc::clone(&self.library))?,
        };
        let result = f(&mut compiler);
        self.lock().push(compiler);
        Ok(result)
    }

    /// Creates sessions up front so that the first compiles do not pay for them.
    pub fn warm_up(&self, count: usize) -> Result<(), ShaderError> {
        let missing = count.saturating_sub(self.idle_count());
        for _ in 0..missing {
            let compiler = Compiler::new(Arc::clone(&self.library))?;
            self.lock().push(compiler);
        }
        Ok(())
    }

    /// Destroys idle sessions beyond `keep`, returning their memory.
    pub fn trim(&self, keep: usize) {
        let excess: Vec<Compiler> = {
            let mut idle = self.lock();
            let keep = keep.min(idle.len());
            idle.drain(keep..).collect()
        };
        // Destroyed outside the lock.
        drop(excess);
    }

    /// Number of idle sessions.
    pub fn idle_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Compiler>> {
        // A panic while holding the lock cannot leave the vector inconsistent.
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
