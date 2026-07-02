/*
 * Copyright (c) 2026 New Vector Ltd.
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial.
 * Please see LICENSE files in the repository root for full details.
 */

package io.element.android.libraries.slashcommands.impl

import dev.zacsweers.metro.Inject
import io.element.android.libraries.matrix.api.room.JoinedRoom
import io.element.android.libraries.slashcommands.api.BotCommandSuggestion
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import timber.log.Timber

/**
 * Reads bot slash commands from `m.room.bot.options` room state events.
 *
 * Each bot posts one state event using its own Matrix user ID as the state key,
 * so multiple bots in the same room don't overwrite each other.
 *
 * Event content schema:
 * ```json
 * {
 *   "commands": [
 *     { "command": "/help",   "description": "Show command reference" },
 *     { "command": "/model",  "parameters": "<profile>", "description": "Switch LLM profile" }
 *   ]
 * }
 * ```
 */
@Inject
class BotCommandDataSource(
    private val room: JoinedRoom,
) {
    companion object {
        const val BOT_OPTIONS_EVENT_TYPE = "m.room.bot.options"
    }

    @Serializable
    private data class BotOptionsContent(
        @SerialName("commands") val commands: List<BotCommandEntry> = emptyList(),
    )

    @Serializable
    private data class BotCommandEntry(
        @SerialName("command") val command: String,
        @SerialName("parameters") val parameters: String? = null,
        @SerialName("description") val description: String,
    )

    private val json = Json { ignoreUnknownKeys = true }

    /**
     * Returns all bot commands published in this room, across all bots.
     * Returns an empty list on any error (missing event, malformed JSON, etc.).
     */
    suspend fun getCommands(): List<BotCommandSuggestion> {
        return try {
            // getStateEvents returns all state events of a given type, keyed by state_key.
            // Each bot uses its user ID as the state_key, so we get one entry per bot.
            val stateEvents = room.getStateEvents(BOT_OPTIONS_EVENT_TYPE)
            stateEvents.flatMap { (botUserId, rawJson) ->
                parseCommandsFromContent(rawJson, botUserId)
            }
        } catch (e: Exception) {
            Timber.d(e, "Failed to read bot commands from room state")
            emptyList()
        }
    }

    private fun parseCommandsFromContent(
        rawJson: String,
        botUserId: String,
    ): List<BotCommandSuggestion> {
        return try {
            val content = json.decodeFromString<BotOptionsContent>(rawJson)
            content.commands
                .filter { it.command.startsWith("/") && it.description.isNotBlank() }
                .map { entry ->
                    BotCommandSuggestion(
                        command = entry.command,
                        parameters = entry.parameters,
                        description = entry.description,
                        botUserId = botUserId,
                    )
                }
        } catch (e: Exception) {
            Timber.d(e, "Failed to parse bot commands for $botUserId")
            emptyList()
        }
    }
}
