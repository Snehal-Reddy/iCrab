# 🦀 iCrab

![iCrab](icrab.png)

> **A minimal, 100% private AI assistant running on a repurposed iPhone.**

**iCrab** is a stripped down minimal personal AI assistant designed to run exclusively on an old iPhone inside [iSH](https://ish.app/). You interact with it via Telegram. It treats your Obsidian vault (or any local directory) as its workspace—giving it native access to read your notes, logs etc.

It is built to stay small, extremely private, and effortlessly extensible.

**Key Highlights:**
- **Zero Ingress:** No webhooks, no open ports. Uses Telegram long-polling.
- **Single Static Binary:** Written in Rust, compiled to `i686-unknown-linux-musl`. **Extremely lightweight (~4.9MB).**
- **Own Your Data:** Everything stays in your workspace. Chat history is stored locally in SQLite.
- **Skill System:** Teach it new skills by simply dropping a `SKILL.md` file in a folder. No plugins, no code required.

---

## 💡 Why iCrab?

I tried a few options of OpenClaw alternatives. They were bloated with complex dependanices that were either unnecessary, or simply wouldn't work on 32 bit Alpine Linux that is emulated via iSH. I wanted something that actually runs *where I am*, uses the notes I already keep, and doesn't depend on a cloud data lock-in, and is dead simple.

---

## ✨ Features

- **Telegram Interface:** Chat natively. It reads, thinks, uses tools, and replies.
- **Workspace Integration:** Reads and writes files directly in your Obsidian vault. It knows what you wrote yesterday because it can read your daily notes.
- **Local Memory & Search:** SQLite-backed persistence with FTS5. The agent can search your entire vault or your chat history blazingly fast.
- **Background Subagents:** Tell it to "Search the web for X and summarize it." It spawns a background agent, freeing up the main chat, and messages you when it's done.
- **Cron & Heartbeat:** Schedule recurring tasks or reminders (e.g., "Summarize unread messages every hour").
- **Basic Tools:**
  - `read_file`, `write_file`, `edit_file`, `append_file`, `list_dir`
  - `web_search` (Brave API or DuckDuckGo) & `web_fetch`
  - `cron` management
  - Restricted `exec` (e.g., for `git pull` syncing)

---

## 🚀 Getting Started

To get started, you'll need an old iPhone with the iSH app installed, and a machine to compile the binary.

### 1. Prep your iPhone (The "Lethal Trifecta")
Read Simon Willison's [lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/) on device privacy.

1. **Sign out of iCloud:** `Settings > [Your Name] > Sign Out`. Disables Activation Lock.
2. **Erase All Content:** Destroys encryption keys, rendering old data unrecoverable.
3. **Do not sign in again:** Choose "Set Up Later in Settings." This stops background sync from hogging RAM.
4. **Disable extras:** Say No to Face ID, Siri, Screen Time, and Analytics.

Download **iSH** from the App Store (use a burner account if desired).

### 2. Create your Telegram Bot
1. Open Telegram and message [@BotFather](https://t.me/botfather).
2. Send `/newbot` and follow the steps to get your **Bot Token**.
3. Find your personal Telegram User ID (e.g., by messaging `@userinfobot`). You'll need this to ensure *only you* can talk to your bot.

### 3. Build iCrab (On your computer)
Because iSH runs a 32-bit x86 environment (`i686`), you must cross-compile.

```bash
# Clone the repository
git clone https://github.com/Snehal-Reddy/iCrab.git
cd iCrab

# Install the cross-compiler tool
cargo install cross

# Build the release binary
cross build --release
```
The compiled binary will be at `target/i686-unknown-linux-musl/release/icrab`. Transfer this file to your iPhone (via SSH into iSH, or start a local server and `curl`).

### 4. Configuration
Create a config file in your iSH environment at `~/.icrab/config.toml`:

```toml
# ~/.icrab/config.toml
workspace = "~/.icrab/workspace"  # Point this to your Obsidian vault
restrict-to-workspace = true

[telegram]
bot-token = "YOUR_TELEGRAM_BOT_TOKEN"
allowed-user-ids = [123456789] # Your Telegram User ID

[llm]
provider = "openrouter"
api-base = "https://openrouter.ai/api/v1"
api-key = "YOUR_LLM_API_KEY"
model = "google/gemini-3-flash-preview" # Or your preferred model

[heartbeat]
interval-minutes = 30
timezone = "Europe/London" # Your IANA timezone
```

*(Note: You can also set secrets as environment variables like `TELEGRAM_BOT_TOKEN` and `ICRAB_LLM_API_KEY`.)*

### 5. Run it!
Inside iSH:
```bash
# Make it executable
chmod +x icrab

# Run it
./icrab
```
Now, open Telegram, find your bot, and say "Hello"!

---

## 🧠 Teaching iCrab New Skills

iCrab is designed to be easily extensible without writing code. To add a skill, simply create a folder in `workspace/skills/` and drop a `SKILL.md` file inside it.

```text
workspace/
└── skills/
    └── workout_logger/
        └── SKILL.md
```