//! The GPU memory budget (ADR 0076): one owner that every streaming system asks how much
//! memory it may keep resident.
//!
//! - **Where the total comes from.** The OS gives each process a budget per memory heap
//!   (`VK_EXT_memory_budget`, through [`Allocator::budget`]); on Windows it is the WDDM
//!   video memory manager's budget, which shrinks when other processes need video memory.
//!   The game takes a share of the device-local heaps' budget ([`BudgetSettings::share`],
//!   with an optional cap), and what the rest of the process already uses (render targets,
//!   the swapchain, geometry that is not streamed, the driver's own) comes off it. What
//!   remains is the **streaming pool**.
//! - **Clients.** Each streaming system ([`GpuBudget::register`]: textures, meshes) reports
//!   every frame what it holds and what it would like to hold, in three classes
//!   ([`Demand`]): **required** (never evicted: the smallest levels that keep everything
//!   drawable), **visible** (what the GPU sampled or drew recently, wanted at full
//!   detail) and **cached** (resident but not seen recently: kept while there is room).
//! - **Grants.** The pool is handed out class by class across the clients: every client's
//!   required bytes first (even beyond the pool: nothing visible may disappear), then the
//!   visible demands (scaled down together when they do not fit), then the cached ones.
//!   A client keeps its resident bytes within its [`GpuBudget::grant`] by dropping its
//!   least important data first (textures their finest levels).
//! - **Between queries** the OS usage is estimated as the last reading plus what the
//!   allocator's own bytes did since (VMA does the same), so the driver call runs every
//!   [`BudgetSettings::query_interval`] frames.
//!
//! The budget allocates nothing after [`GpuBudget::register`].

use engine_rhi::{AdapterKind, Allocator, Device, HeapBudget};

/// Most clients a [`GpuBudget`] serves.
pub const MAX_CLIENTS: usize = 8;

/// How much of the OS budget the game takes (ADR 0076).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetSettings {
    /// Share of the device-local heaps' OS budget the game may use, 0 … 1.
    pub share: f32,
    /// Upper bound on that, in bytes (`None`: none).
    pub cap: Option<u64>,
    /// The streaming pool, fixed (`None`: from the OS budget). For a low-memory setting,
    /// and for tests of behavior under pressure.
    pub streaming_limit: Option<u64>,
    /// Frames between OS queries (at least 1); the usage is estimated in between.
    pub query_interval: u32,
}

impl BudgetSettings {
    /// Defaults for an adapter kind: 90% of the budget on a discrete GPU (the OS and other
    /// applications keep the rest); on an integrated GPU, whose device-local heap is system
    /// memory the CPU side also needs (22 GB of the Radeon's 36 GB on ref-1), half of it.
    pub fn for_adapter(kind: AdapterKind) -> Self {
        Self {
            share: if kind == AdapterKind::Integrated {
                0.5
            } else {
                0.9
            },
            cap: None,
            streaming_limit: None,
            query_interval: 8,
        }
    }
}

impl Default for BudgetSettings {
    /// [`Self::for_adapter`] for a discrete GPU.
    fn default() -> Self {
        Self::for_adapter(AdapterKind::Discrete)
    }
}

/// What a client would like resident, by class, in bytes (see the module documentation).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Demand {
    /// Never evicted.
    pub required: u64,
    /// Seen recently: wanted at the detail the GPU asked for.
    pub visible: u64,
    /// Resident but not seen recently.
    pub cached: u64,
}

impl Demand {
    /// All three classes.
    pub fn total(&self) -> u64 {
        self.required + self.visible + self.cached
    }
}

/// A client of a [`GpuBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u8);

#[derive(Debug, Clone, Copy, Default)]
struct Client {
    name: &'static str,
    resident: u64,
    demand: Demand,
    grant: u64,
}

/// One client's numbers, for overlays and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientStats {
    /// The name it registered with.
    pub name: &'static str,
    /// Bytes it holds.
    pub resident: u64,
    /// What it would like.
    pub demand: Demand,
    /// What it may hold.
    pub grant: u64,
}

/// The budget's numbers, for overlays and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetStats {
    /// The device-local heaps at the last OS query (usage estimated since).
    pub device_local: HeapBudget,
    /// The game's share of the budget.
    pub total: u64,
    /// The process's usage outside the streaming clients.
    pub other: u64,
    /// What the streaming clients may hold together.
    pub pool: u64,
    /// What the clients hold.
    pub resident: u64,
    /// What they would like.
    pub demand: u64,
}

/// The GPU memory budget (see the module documentation).
#[derive(Debug, Clone)]
pub struct GpuBudget {
    settings: BudgetSettings,
    clients: [Client; MAX_CLIENTS],
    count: usize,
    /// The device-local heaps at the last query, and the allocator's bytes then.
    queried: HeapBudget,
    frames_since_query: u32,
    stats: BudgetStats,
}

impl GpuBudget {
    /// A budget with [`BudgetSettings::for_adapter`] of `device`'s adapter.
    pub fn new(device: &Device) -> Self {
        Self::with_settings(BudgetSettings::for_adapter(device.adapter().kind))
    }

    /// A budget with explicit settings.
    pub fn with_settings(settings: BudgetSettings) -> Self {
        Self {
            settings,
            clients: [Client::default(); MAX_CLIENTS],
            count: 0,
            queried: HeapBudget::default(),
            frames_since_query: u32::MAX,
            stats: BudgetStats::default(),
        }
    }

    /// The settings.
    pub fn settings(&self) -> BudgetSettings {
        self.settings
    }

    /// Changes the settings; grants follow at the next [`Self::update`].
    pub fn set_settings(&mut self, settings: BudgetSettings) {
        self.settings = settings;
        self.frames_since_query = u32::MAX;
    }

    /// Adds a client (at most [`MAX_CLIENTS`]; `None` beyond).
    pub fn register(&mut self, name: &'static str) -> Option<ClientId> {
        if self.count == MAX_CLIENTS {
            return None;
        }
        self.clients[self.count] = Client {
            name,
            ..Client::default()
        };
        self.count += 1;
        Some(ClientId(self.count as u8 - 1))
    }

    /// A client's report: the bytes it holds and what it would like. Grants follow at the
    /// next [`Self::update`].
    pub fn report(&mut self, client: ClientId, resident: u64, demand: Demand) {
        if let Some(c) = self.clients[..self.count].get_mut(usize::from(client.0)) {
            c.resident = resident;
            c.demand = demand;
        }
    }

    /// What `client` may hold, from the last [`Self::update`].
    pub fn grant(&self, client: ClientId) -> u64 {
        self.clients[..self.count]
            .get(usize::from(client.0))
            .map_or(0, |c| c.grant)
    }

    /// A client's numbers.
    pub fn client(&self, client: ClientId) -> Option<ClientStats> {
        let c = self.clients[..self.count].get(usize::from(client.0))?;
        Some(ClientStats {
            name: c.name,
            resident: c.resident,
            demand: c.demand,
            grant: c.grant,
        })
    }

    /// The numbers of the last [`Self::update`].
    pub fn stats(&self) -> BudgetStats {
        self.stats
    }

    /// Reads the OS budget (every [`BudgetSettings::query_interval`] frames; estimated in
    /// between) and hands out the grants. Once per frame, after the clients' reports.
    pub fn update(&mut self, allocator: &Allocator) {
        engine_profiler::zone!("render::GpuBudget::update");
        let interval = self.settings.query_interval.max(1);
        let local = if self.frames_since_query >= interval - 1 {
            self.frames_since_query = 0;
            let local = allocator.budget().device_local();
            self.queried = local;
            local
        } else {
            self.frames_since_query += 1;
            // The OS reading plus what this allocator did since.
            let allocated = allocator.device_local_bytes();
            let mut local = self.queried;
            local.usage = (local.usage + allocated).saturating_sub(self.queried.allocated);
            local.allocated = allocated;
            local
        };
        self.distribute(local);
    }

    /// Hands out the grants for a reading of the device-local heaps.
    fn distribute(&mut self, local: HeapBudget) {
        let s = self.settings;
        let share = (local.budget as f64 * f64::from(s.share.clamp(0.0, 1.0))) as u64;
        let total = s.cap.map_or(share, |cap| share.min(cap));
        let clients = &mut self.clients[..self.count];
        let resident: u64 = clients.iter().map(|c| c.resident).sum();
        let other = local.usage.saturating_sub(resident);
        let pool = s
            .streaming_limit
            .unwrap_or_else(|| total.saturating_sub(other));
        let sum = |f: fn(&Demand) -> u64| clients.iter().map(|c| f(&c.demand)).sum::<u64>();
        let (required, visible, cached) =
            (sum(|d| d.required), sum(|d| d.visible), sum(|d| d.cached));
        // Class by class: required always, then visible and cached scaled to what is left.
        let mut left = pool.saturating_sub(required);
        let visible_part = left.min(visible);
        left -= visible_part;
        let cached_part = left.min(cached);
        let scale = |part: u64, of: u64, x: u64| {
            if of == 0 {
                0
            } else {
                (u128::from(x) * u128::from(part) / u128::from(of)) as u64
            }
        };
        for c in clients.iter_mut() {
            c.grant = c.demand.required
                + scale(visible_part, visible, c.demand.visible)
                + scale(cached_part, cached, c.demand.cached);
        }
        self.stats = BudgetStats {
            device_local: local,
            total,
            other,
            pool,
            resident,
            demand: required + visible + cached,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heap(budget: u64, usage: u64) -> HeapBudget {
        HeapBudget {
            size: budget,
            device_local: true,
            budget,
            usage,
            allocated: usage,
        }
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn grants_go_class_by_class() {
        let mut b = GpuBudget::with_settings(BudgetSettings {
            share: 0.5,
            ..BudgetSettings::default()
        });
        let textures = b.register("textures").unwrap();
        let meshes = b.register("meshes").unwrap();
        // 1000 MiB budget, half for the game; 100 MiB used besides the clients' 150.
        b.report(
            textures,
            100 * MIB,
            Demand {
                required: 20 * MIB,
                visible: 300 * MIB,
                cached: 50 * MIB,
            },
        );
        b.report(
            meshes,
            50 * MIB,
            Demand {
                required: 10 * MIB,
                visible: 100 * MIB,
                cached: 0,
            },
        );
        b.distribute(heap(1000 * MIB, 250 * MIB));
        let s = b.stats();
        assert_eq!(
            (s.total, s.other, s.pool),
            (500 * MIB, 100 * MIB, 400 * MIB)
        );
        // 400 − 30 required = 370 for 400 of visible demand: scaled by 370/400.
        assert_eq!(b.grant(textures), 20 * MIB + 300 * MIB * 370 / 400);
        assert_eq!(b.grant(meshes), 10 * MIB + 100 * MIB * 370 / 400);
        // Plenty: everything, cached included.
        b.distribute(heap(4000 * MIB, 250 * MIB));
        assert_eq!(b.grant(textures), 370 * MIB);
        assert_eq!(b.grant(meshes), 110 * MIB);
    }

    #[test]
    fn required_bytes_are_granted_even_beyond_the_pool() {
        let mut b = GpuBudget::with_settings(BudgetSettings {
            streaming_limit: Some(8 * MIB),
            ..BudgetSettings::default()
        });
        let t = b.register("textures").unwrap();
        b.report(
            t,
            0,
            Demand {
                required: 20 * MIB,
                visible: 100 * MIB,
                cached: 0,
            },
        );
        b.distribute(heap(16_000 * MIB, 0));
        assert_eq!(b.stats().pool, 8 * MIB);
        assert_eq!(b.grant(t), 20 * MIB);
    }

    #[test]
    fn the_cap_and_registration_limits_hold() {
        let mut b = GpuBudget::with_settings(BudgetSettings {
            share: 1.0,
            cap: Some(64 * MIB),
            ..BudgetSettings::default()
        });
        for _ in 0..MAX_CLIENTS {
            assert!(b.register("x").is_some());
        }
        assert!(b.register("one too many").is_none());
        b.distribute(heap(1000 * MIB, 16 * MIB));
        assert_eq!(b.stats().total, 64 * MIB);
        assert_eq!(b.stats().pool, 48 * MIB);
    }
}
