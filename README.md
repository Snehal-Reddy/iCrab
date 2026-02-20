# iCrab

![iCrab](icrab.png)

iCrab is a personal AI assistant that runs on an old iPhone inside iSH. You talk to it over Telegram. It uses your Obsidian vault (or any folder you point it at) as its workspace, so it can read your notes, log stuff, and search your daily logs. **One static binary, no webhooks, no open ports**. The kind of thing you run on a phone that was otherwise sitting in a drawer.

It is built to stay small and easy to extend.

## Why it exists

I wanted something that actually runs where I am, uses the notes I already keep, and does not depend on a cloud service. iSH on an iPhone gives you a 32-bit Linux userspace. iCrab is a single binary for that environment: Telegram long-poll in, agent loop with tools, everything stored in your workspace. Add a skill by dropping a `SKILL.md` in a folder. No plugins, no DLLs.

## Getting started

You need an iPhone with iSH and a stripped-down setup so the phone is not busy with iCloud and other services.

**Pre-setup (iPhone wipe).** I won't go into privacy here, but read [the lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/) from Simon Willison. Wipe anything important off the iPhone first.

- **Sign out of iCloud:** `Settings > [Your Name] > Sign Out`. That removes the device from Find My and disables Activation Lock.
- **Erase All Content and Settings:** On modern iPhones this destroys the encryption keys; the data on the chip becomes unrecoverable.
- **Do not sign in to iCloud again:** When it asks for an Apple ID, choose "Forgot password or don't have an Apple ID" then "Set Up Later in Settings." Keeps the phone light and stops background sync from hogging RAM.
- **Disable everything:** During setup, say No to Face ID, Siri, Screen Time, and Analytics. You want the CPU available for iSH.
- Optional: use a burner Apple account for the App Store only.

**Setup.** Download iSH on the iPhone.

**Build (on a real machine, from this directory `iCrab/`):**

- Install [cross](https://github.com/cross-rs/cross): `cargo install cross`
- `cross build` for debug, `cross build --release` for the binary you deploy.

Output: `target/i686-unknown-linux-musl/debug/icrab` or `.../release/icrab`. Put that in iSH, add your config (see below), and run it.

**Config:** One file, `~/.icrab/config.toml`. You set the workspace path (your Obsidian vault or a clone), your Telegram bot token, and the LLM (OpenRouter, OpenAI-compatible, etc.). Optional: Brave API key for web search, heartbeat interval, cron. No secrets in the binary; use the config file or env vars.

## What it can do

- **Chat over Telegram:** Long-poll only. You message the bot; it runs the agent and replies. No webhooks, no open ports.
- **Use your workspace:** Read and write files under the workspace (e.g. daily logs, workouts,  notes). File tools are restricted to that tree.
- **Remember and search:** MEMORY.md and daily notes are in context. Persistence (in progress) adds SQLite-backed chat history and FTS5 search over your vault so the agent can find "what did we say about X" quickly.
- **Tools:** File (read, write, list, edit, append), web search and fetch, cron (reminders and one-off tasks), and a `message` tool to send you text. Optional: restricted `exec` for things like `git pull` in the vault.
- **Subagents:** The agent can spawn a background task (e.g. "search the web and summarize"). The subagent runs with the same tools except no spawn; it reports back via `message` so you get the result in Telegram without blocking the main chat.
- **Heartbeat:** A timer can read a file (e.g. `HEARTBEAT.md`) and run the agent on a schedule (e.g. "check my calendar" or "summarize unread").
- **Skills:** Put a `SKILL.md` in `workspace/skills/<name>/`. The agent sees the list and can open a skill’s doc when it needs it. Extending is "add a folder and a markdown file."

So: simple (one binary, one config, one ingress), light (static musl, minimal deps), and expandable (skills as docs, optional tools). If that matches what you want from a phone-bound assistant, the rest is in the design and persistence docs.
