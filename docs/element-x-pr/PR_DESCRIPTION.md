# feat: Show bot slash commands in composer autocomplete

## Summary

When a bot publishes its commands via a room state event, show those commands in
the `/` autocomplete alongside Element's built-in commands.

This requires no server changes and no new Matrix spec — bots post a standard
room state event that clients read. The `SlashCommandService` interface and
`SlashCommandSuggestion` model already have exactly the right shape.

## State event schema

Event type: `m.room.bot.options`  
State key: the bot's Matrix user ID (allows multiple bots per room without conflict)

Content:
```json
{
  "commands": [
    { "command": "/help",   "description": "Show command reference" },
    { "command": "/status", "description": "Uptime, model, history stats, active jobs" },
    { "command": "/model",  "parameters": "<profile>", "description": "Switch LLM profile for this room" },
    { "command": "/cancel", "parameters": "<job-id>",  "description": "Abort a background job" },
    { "command": "/clear",  "description": "Wipe conversation history" },
    { "command": "/jobs",   "description": "List background jobs" }
  ]
}
```

- `command` — the slash command string including the `/`
- `parameters` — optional arg hint shown in the suggestion UI (same as `Command.parameters`)
- `description` — one-line description shown in the suggestion UI

## Files changed

| File | Change |
|---|---|
| `libraries/slashcommands/api/.../BotCommandSuggestion.kt` | New data class for the parsed state event content |
| `libraries/slashcommands/impl/.../BotCommandDataSource.kt` | New: reads `m.room.bot.options` state events from the room |
| `libraries/slashcommands/impl/.../DefaultSlashCommandService.kt` | Inject `BotCommandDataSource`, merge results in `getSuggestions()` |
| `libraries/slashcommands/impl/.../DefaultSlashCommandServiceTest.kt` | Unit tests for merged suggestions and filtering |

## What does NOT change

- Built-in command parsing/execution — bot commands are display-only hints; sending
  a bot command is just sending a regular text message (the bot handles it)
- The `Command` enum — bot commands appear after built-in ones in the list
- Any server-side code
- The `SlashCommandService` interface

## Security note

Bot commands are read from room state events which require room membership to post.
The content is treated as display strings only — no execution path exists for bot
command entries (they resolve to a plain text send, same as typing the command
manually). The bot command list is filtered by the user's typed prefix the same
way built-in commands are.
