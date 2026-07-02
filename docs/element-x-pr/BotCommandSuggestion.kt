/*
 * Copyright (c) 2026 New Vector Ltd.
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial.
 * Please see LICENSE files in the repository root for full details.
 */

package io.element.android.libraries.slashcommands.api

/**
 * A slash command suggestion sourced from a bot's room state event
 * (`m.room.bot.options`), as opposed to Element's built-in [SlashCommandSuggestion]s.
 *
 * Both types map to [SlashCommandSuggestion] for display; this intermediary type
 * carries the origin so we can tag bot commands visually in the UI if desired.
 */
data class BotCommandSuggestion(
    /** The slash command string, including the leading `/`. E.g. `/help`. */
    val command: String,
    /** Optional parameter hint. E.g. `<profile>`. */
    val parameters: String?,
    /** One-line description shown in the autocomplete row. */
    val description: String,
    /** The Matrix user ID of the bot that published this command. */
    val botUserId: String,
)
