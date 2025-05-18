#[cfg(feature = "buddy-alloc")]
mod alloc;
mod game;
mod palette;
mod snake;
mod wasm4;
use game::Game;
use lazy_static::lazy_static;
use std::sync::Mutex;

lazy_static! {
    static ref SNAKE_GAME: Mutex<Game> = Mutex::new(Game::new());
}

#[no_mangle]
fn start() {
    palette::set_palette([0xfff6d3, 0xf9a875, 0xeb6b6f, 0x7c3f58]);
}

#[no_mangle]
fn update() {
    SNAKE_GAME.lock().expect("game_state").update();
}

#[repr(C)]
pub struct GameDump {
    pub score: u32,
    pub body_len: u32,
    pub body: [snake::Point; 64],
    pub fruit: snake::Point,
}

static mut LAST_DUMP: GameDump = GameDump {
    score: 0,
    body_len: 0,
    body: [snake::Point { x: 0, y: 0 }; 64],
    fruit: snake::Point { x: 0, y: 0 },
};

#[no_mangle]
pub extern "C" fn play_dump(inputs_ptr: *const u8, len: usize) -> *const GameDump {
    let inputs = unsafe { core::slice::from_raw_parts(inputs_ptr, len) };
    let mut game = Game::new();

    for &b in inputs {
        let btn = match b {
            b'L' => wasm4::BUTTON_LEFT,
            b'R' => wasm4::BUTTON_RIGHT,
            b'U' => wasm4::BUTTON_UP,
            b'D' => wasm4::BUTTON_DOWN,
            _ => 0,
        };
        unsafe { (wasm4::GAMEPAD1 as *mut u8).write(btn) };
        game.update();
        unsafe { (wasm4::GAMEPAD1 as *mut u8).write(0) };
    }

    unsafe {
        LAST_DUMP.score = game.score;
        LAST_DUMP.body_len = game.snake.body.len() as u32;
        for (dst, src) in LAST_DUMP.body.iter_mut().zip(game.snake.body.iter()) {
            *dst = *src;
        }
        LAST_DUMP.fruit = game.fruit;
        &LAST_DUMP as *const GameDump
    }
}
