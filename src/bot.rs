use anyhow::Result;
use azalea::{app::PluginGroup, prelude::*};
use flint_core::test_spec::BlockFace;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

// Constants for connection and timing
const INIT_WAIT_ATTEMPTS: u32 = 50;
const INIT_WAIT_DELAY_MS: u64 = 100;
const GAME_STATE_WAIT_ATTEMPTS: u32 = 100;
const WORLD_SYNC_DELAY_MS: u64 = 500;
const DEFAULT_CHUNK_LOAD_DELAY_MS: u64 = 0;
const TELEPORT_NEAR_THRESHOLD: i32 = 32;
const INTERACT_WAIT_DELAY_MS: u64 = 50;

type ChatReceiver = std::sync::mpsc::Receiver<(Option<String>, String)>;

#[derive(Clone, Component)]
struct State {
    client_handle: Arc<RwLock<Option<Client>>>,
    in_game: Arc<AtomicBool>,
    chat_tx: Option<std::sync::mpsc::Sender<(Option<String>, String)>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            client_handle: Arc::new(RwLock::new(None)),
            in_game: Arc::new(AtomicBool::new(false)),
            chat_tx: None,
        }
    }
}

#[derive(Clone)]
pub struct TestBot {
    client: Option<Arc<RwLock<Option<Client>>>>,
    in_game: Option<Arc<AtomicBool>>,
    chat_rx: Option<Arc<parking_lot::Mutex<ChatReceiver>>>,
    last_near: Arc<parking_lot::Mutex<Option<[i32; 3]>>>,
    chunk_load_delay_ms: Arc<AtomicU64>,
}

impl Default for TestBot {
    fn default() -> Self {
        Self {
            client: None,
            in_game: None,
            chat_rx: None,
            last_near: Arc::new(parking_lot::Mutex::new(None)),
            chunk_load_delay_ms: Arc::new(AtomicU64::new(DEFAULT_CHUNK_LOAD_DELAY_MS)),
        }
    }
}

impl TestBot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a reference to the client, or error if not connected
    fn get_client(&self) -> Result<parking_lot::RwLockReadGuard<'_, Option<Client>>> {
        self.client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Bot not connected"))
            .map(|handle| handle.read())
    }

    pub fn connect(&mut self, server: &str) -> Result<()> {
        let account = Account::offline("flintmc_testbot");

        tracing::info!("Connecting to server: {}", server);

        // Create chat channel
        let (chat_tx, chat_rx) = std::sync::mpsc::channel();

        let state = State {
            chat_tx: Some(chat_tx),
            ..Default::default()
        };
        let client_handle = state.client_handle.clone();
        let in_game = state.in_game.clone();

        // Spawn the bot in a background thread with LocalSet (required by new azalea version)
        let server_owned = server.to_string();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                async fn handler(bot: Client, event: Event, state: State) -> Result<()> {
                    match event {
                        Event::Init => {
                            *state.client_handle.write() = Some(bot.clone());
                            tracing::info!("Bot initialized");
                        }
                        Event::Login => {
                            // Login event means we're fully in the game state
                            state.in_game.store(true, Ordering::SeqCst);
                            tracing::info!("Bot in game state");
                        }
                        Event::Chat(m) => {
                            // Extract the message content
                            let message = m.message().to_string();
                            // Try to get sender name (best effort)
                            // Fallback: parse "<Name>"
                            let sender = if message.starts_with('<') {
                                message.find('>').map(|end| message[1..end].to_string())
                            } else {
                                None
                            };

                            if let Some(ref tx) = state.chat_tx {
                                let _ = tx.send((sender, message));
                            }
                        }
                        _ => {}
                    }
                    Ok(())
                }

                let result = ClientBuilder::new_without_plugins()
                    .add_plugins(
                        azalea::DefaultPlugins
                            .build()
                            .disable::<azalea::physics::PhysicsPlugin>(),
                    )
                    .add_plugins(azalea::bot::DefaultBotPlugins)
                    .set_handler(handler)
                    .set_state(state)
                    .start(account, server_owned.as_str())
                    .await;

                if let AppExit::Error(e) = result {
                    tracing::error!("Bot connection error: {}", e);
                }
            });
        });

        // Wait for client to initialize
        for _ in 0..INIT_WAIT_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(INIT_WAIT_DELAY_MS));
            if client_handle.read().is_some() {
                break;
            }
        }

        if client_handle.read().is_none() {
            anyhow::bail!("Failed to initialize bot connection");
        }

        // Wait for bot to be in game state
        tracing::info!("Waiting for bot to enter game state...");
        for _ in 0..GAME_STATE_WAIT_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(INIT_WAIT_DELAY_MS));
            if in_game.load(Ordering::SeqCst) {
                break;
            }
        }

        if !in_game.load(Ordering::SeqCst) {
            anyhow::bail!("Bot failed to enter game state within timeout");
        }

        self.client = Some(client_handle);
        self.in_game = Some(in_game);
        self.chat_rx = Some(Arc::new(parking_lot::Mutex::new(chat_rx)));
        self.last_near = Arc::new(parking_lot::Mutex::new(None));
        tracing::info!("Connected successfully and in game state");

        // Give a small amount of extra time for world data to sync
        std::thread::sleep(std::time::Duration::from_millis(WORLD_SYNC_DELAY_MS));

        Ok(())
    }

    /// Wait for a chat message with timeout
    pub fn recv_chat_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Option<(Option<String>, String)> {
        if let Some(ref rx_mutex) = self.chat_rx {
            let rx = rx_mutex.lock();
            rx.recv_timeout(timeout).ok()
        } else {
            None
        }
    }

    pub fn send_command(&self, command: &str) -> Result<()> {
        let client_guard = self.get_client()?;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Bot not initialized"))?;

        // Add "/" prefix if not present
        let command_with_slash = if command.starts_with('/') {
            command.to_string()
        } else {
            format!("/{}", command)
        };
        tracing::debug!("Sending command: {}", command_with_slash);
        client.chat(&command_with_slash);
        Ok(())
    }

    pub fn set_direction(&self, x_rot: f32, y_rot: f32) -> Result<()> {
        let client_guard = self.get_client()?;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Bot not initialized"))?;

        tracing::debug!("Setting direction: {} {}", x_rot, y_rot);

        client.set_direction(x_rot, y_rot)?;
        Ok(())
    }

    pub fn block_interact(&self, pos: [i32; 3]) -> Result<()> {
        let client_guard = self.get_client()?;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Bot not initialized"))?;

        let block_pos = azalea::BlockPos::from(<(i32, i32, i32)>::from(pos));

        tracing::debug!("Interacting with block: {}", block_pos);
        client.block_interact(block_pos);

        Ok(())
    }

    pub fn prepare_for_interact_face(&self, pos: [i32; 3], block_face: BlockFace) -> Result<()> {
        // Move 2 blocks away to allow placing blocks with this and -1 on y, so that the head is
        // aligned with the block and not the legs.
        let tp_offset = match block_face {
            BlockFace::Top => [0, 2, 0],
            BlockFace::Bottom => [0, -3, 0],
            BlockFace::North => [0, -1, -2],
            BlockFace::South => [0, -1, 2],
            BlockFace::East => [2, -1, 0],
            BlockFace::West => [-2, -1, 0],
        };
        let tp_pos = [
            pos[0] + tp_offset[0],
            pos[1] + tp_offset[1],
            pos[2] + tp_offset[2],
        ];

        let (x_rot, y_rot) = match block_face {
            BlockFace::Top => (0.0, 90.0),
            BlockFace::Bottom => (0.0, -90.0),
            BlockFace::North => (0.0, 0.0),
            BlockFace::South => (180.0, 0.0),
            BlockFace::East => (90.0, 0.0),
            BlockFace::West => (-90.0, 0.0),
        };

        self.teleport(tp_pos, false)?;

        self.set_direction(x_rot, y_rot)?;

        std::thread::sleep(Duration::from_millis(INTERACT_WAIT_DELAY_MS));

        Ok(())
    }

    pub fn teleport(&self, pos: [i32; 3], instant: bool) -> Result<()> {
        self.send_command(&format!(
            "tp flintmc_testbot {} {} {}",
            pos[0], pos[1], pos[2]
        ))?;

        if instant {
            return Ok(());
        }

        let delay = self.chunk_load_delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }

        Ok(())
    }

    /// Teleport the bot near `pos` so the client loads chunks and the server simulates blocks.
    /// When `force` is false, skips the teleport if already within `TELEPORT_NEAR_THRESHOLD`.
    pub fn ensure_near(&self, pos: [i32; 3]) -> Result<()> {
        self.ensure_near_inner(pos, false)
    }

    /// Always teleport, ignoring the near-position cache (used when visiting each parallel test).
    pub fn ensure_near_force(&self, pos: [i32; 3]) -> Result<()> {
        self.ensure_near_inner(pos, true)
    }

    fn ensure_near_inner(&self, pos: [i32; 3], force: bool) -> Result<()> {
        let need_tp = if force {
            true
        } else {
            let last = self.last_near.lock();
            match *last {
                Some(p) => {
                    (p[0] - pos[0])
                        .abs()
                        .max((p[1] - pos[1]).abs())
                        .max((p[2] - pos[2]).abs())
                        > TELEPORT_NEAR_THRESHOLD
                }
                None => true,
            }
        };

        if need_tp {
            let tp_y = pos[1].clamp(-60, 320);
            self.teleport([pos[0], tp_y, pos[2]], false)?;

            *self.last_near.lock() = Some(pos);
        }

        Ok(())
    }

    pub fn set_chunk_load_delay_ms(&self, delay_ms: u64) {
        self.chunk_load_delay_ms.store(delay_ms, Ordering::Relaxed);
    }

    pub fn reset_teleport_cache(&self) {
        *self.last_near.lock() = None;
    }

    pub fn get_block(&self, pos: [i32; 3]) -> Result<Option<String>> {
        let client_guard = self.get_client()?;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Bot not initialized"))?;

        let block_pos = azalea::BlockPos::new(pos[0], pos[1], pos[2]);
        if let Ok(world_lock) = client.world() {
            let world = world_lock.read();
            let block_state = world.get_block_state(block_pos);

            if let Some(state) = block_state {
                // Return block state as debug string
                let state_str = format!("{:?}", state);
                Ok(Some(state_str))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Get the bot's current position
    pub fn get_position(&self) -> Result<[i32; 3]> {
        let client_guard = self.get_client()?;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Bot not initialized"))?;

        if let Ok(pos) = client.position() {
            return Ok([pos.x as i32, pos.y as i32, pos.z as i32]);
        }
        Ok([0, 0, 0])
    }
}
