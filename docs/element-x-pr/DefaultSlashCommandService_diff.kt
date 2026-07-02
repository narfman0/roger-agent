/*
 * Diff of changes to DefaultSlashCommandService.kt
 *
 * Only the modified constructor and getSuggestions() are shown.
 * Everything else (parse, proceedSendMessage, proceedAdmin) is unchanged.
 */

@ContributesBinding(RoomScope::class)
class DefaultSlashCommandService(
    private val commandParser: CommandParser,
    private val commandExecutor: CommandExecutor,
    private val stringProvider: StringProvider,
    private val appPreferencesStore: AppPreferencesStore,
    private val featureFlagService: FeatureFlagService,
    private val capabilitiesProvider: HomeserverCapabilitiesProvider,
+   private val botCommandDataSource: BotCommandDataSource,      // NEW
) : SlashCommandService {

    override suspend fun getSuggestions(
        text: String,
        isInThread: Boolean,
    ): List<SlashCommandSuggestion> {
        if (!featureFlagService.isFeatureEnabled(FeatureFlags.SlashCommand)) return emptyList()
        val isDeveloperModeEnabled = appPreferencesStore.isDeveloperModeEnabledFlow().first()

        // --- existing built-in commands (unchanged) ---
        val builtIn = Command.entries
            .asSequence()
            .filter { it.startsWith(text) }
            .filter { !isInThread || it.isAllowedInThread }
            .filter { !it.isDevCommand || isDeveloperModeEnabled }
            .run {
                val canUserChangeDisplayName = withTimeoutOrNull(5.seconds) {
                    capabilitiesProvider.canChangeDisplayName().getOrNull()
                } ?: false
                if (!canUserChangeDisplayName) {
                    filterNot { it == Command.CHANGE_DISPLAY_NAME || it == Command.CHANGE_DISPLAY_NAME_FOR_ROOM }
                } else {
                    this
                }
            }
            .run {
                val canUserChangeAvatar = withTimeoutOrNull(5.seconds) {
                    capabilitiesProvider.canChangeAvatarUrl().getOrNull()
                } ?: false
                if (!canUserChangeAvatar) {
                    filterNot { it == Command.CHANGE_AVATAR || it == Command.CHANGE_AVATAR_FOR_ROOM }
                } else {
                    this
                }
            }
            .map {
                SlashCommandSuggestion(
                    command = it.command,
                    parameters = it.parameters,
                    description = stringProvider.getString(it.description),
                )
            }
            .toList()

+       // --- bot commands from room state (NEW) ---
+       val botCommands = botCommandDataSource.getCommands()
+           .filter { it.command.startsWith(text, ignoreCase = true) }
+           // Skip bot commands that shadow a built-in command
+           .filter { bot -> builtIn.none { it.command == bot.command } }
+           .map { bot ->
+               SlashCommandSuggestion(
+                   command = bot.command,
+                   parameters = bot.parameters,
+                   description = bot.description,
+               )
+           }

+       return builtIn + botCommands
-       return builtIn
    }

    // parse(), proceedSendMessage(), proceedAdmin() — unchanged
}
