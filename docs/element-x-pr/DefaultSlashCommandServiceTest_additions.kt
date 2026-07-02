/*
 * Test additions for DefaultSlashCommandService (bot command cases).
 * Add these to the existing DefaultSlashCommandServiceTest class.
 */

// ── Fakes ────────────────────────────────────────────────────────────────────

class FakeBotCommandDataSource(
    private val commands: List<BotCommandSuggestion> = emptyList(),
) : BotCommandDataSource {
    override suspend fun getCommands() = commands
}

// ── Tests ────────────────────────────────────────────────────────────────────

@Test
fun `bot commands appear in suggestions when prefix matches`() = runTest {
    val service = createService(
        botCommands = listOf(
            BotCommandSuggestion("/help", null, "Show help", "@bot:server"),
            BotCommandSuggestion("/status", null, "Show status", "@bot:server"),
        )
    )
    val result = service.getSuggestions("st", isInThread = false)
    assertThat(result.map { it.command }).contains("/status")
    assertThat(result.map { it.command }).doesNotContain("/help")
}

@Test
fun `bot commands do not shadow built-in commands`() = runTest {
    // /me is a built-in — the bot's /me should not appear
    val service = createService(
        botCommands = listOf(
            BotCommandSuggestion("/me", null, "Bot emote", "@bot:server"),
            BotCommandSuggestion("/roger-status", null, "Roger status", "@bot:server"),
        )
    )
    val result = service.getSuggestions("me", isInThread = false)
    val meEntries = result.filter { it.command == "/me" }
    assertThat(meEntries).hasSize(1)  // only the built-in
}

@Test
fun `empty bot commands list returns only built-ins`() = runTest {
    val service = createService(botCommands = emptyList())
    val result = service.getSuggestions("", isInThread = false)
    assertThat(result).isNotEmpty()
    // All entries should come from Command.entries
    result.forEach { s ->
        assertThat(Command.entries.any { it.command == s.command }).isTrue()
    }
}

@Test
fun `bot commands are case-insensitive prefix matched`() = runTest {
    val service = createService(
        botCommands = listOf(
            BotCommandSuggestion("/Status", null, "Show status", "@bot:server"),
        )
    )
    assertThat(service.getSuggestions("sta", isInThread = false).map { it.command })
        .contains("/Status")
    assertThat(service.getSuggestions("STA", isInThread = false).map { it.command })
        .contains("/Status")
}

// Helper — returns a service wired with the given bot commands.
// Reuses the existing createDefaultService() helper but adds FakeBotCommandDataSource.
private fun createService(
    botCommands: List<BotCommandSuggestion> = emptyList(),
): DefaultSlashCommandService {
    return createDefaultService(
        botCommandDataSource = FakeBotCommandDataSource(botCommands),
    )
}
