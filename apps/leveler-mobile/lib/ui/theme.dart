/// One place to decide how the app looks.
///
/// Material 3's default from a seed colour gives every surface a wash of that
/// colour, which on this app read as a lavender haze behind everything and left
/// cards, bubbles and the background barely distinguishable. The scheme below
/// keeps the seed for accents and pulls the surfaces back to near-neutral, so
/// the things that carry meaning — a bubble, an approval, a status — are the
/// only coloured things on screen.
library;

import 'package:flutter/material.dart';

/// The blue the desktop UI uses, so the two feel like one product.
const Color levelerSeed = Color(0xFF2F6FEB);

ThemeData levelerTheme(Brightness brightness) {
  final dark = brightness == Brightness.dark;
  final scheme = ColorScheme.fromSeed(
    seedColor: levelerSeed,
    brightness: brightness,
  ).copyWith(
    // Near-neutral surfaces: the tint belongs on what the user must notice.
    surface: dark ? const Color(0xFF14161A) : const Color(0xFFFBFBFD),
    surfaceContainerLowest: dark ? const Color(0xFF101215) : Colors.white,
    surfaceContainerHighest: dark ? const Color(0xFF23262C) : const Color(0xFFEFF1F5),
  );

  return ThemeData(
    useMaterial3: true,
    colorScheme: scheme,
    scaffoldBackgroundColor: scheme.surface,
    // A hairline instead of a shadow: the app bar should separate the screen
    // from the title, not float above it.
    appBarTheme: AppBarTheme(
      backgroundColor: scheme.surface,
      surfaceTintColor: Colors.transparent,
      elevation: 0,
      scrolledUnderElevation: 0,
      centerTitle: true,
      shape: Border(bottom: BorderSide(color: scheme.outlineVariant, width: 0.5)),
      titleTextStyle: TextStyle(
        color: scheme.onSurface,
        fontSize: 17,
        fontWeight: FontWeight.w600,
      ),
    ),
    listTileTheme: const ListTileThemeData(
      contentPadding: EdgeInsets.symmetric(horizontal: 16, vertical: 6),
      minVerticalPadding: 10,
    ),
    dividerTheme: DividerThemeData(
      color: scheme.outlineVariant,
      thickness: 0.5,
      space: 0.5,
    ),
    cardTheme: CardThemeData(
      color: scheme.surfaceContainerLowest,
      surfaceTintColor: Colors.transparent,
      elevation: 0,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(16),
        side: BorderSide(color: scheme.outlineVariant, width: 0.5),
      ),
      margin: EdgeInsets.zero,
    ),
    // Dialogs kept Material 3's tinted surface while everything else went
    // neutral, so the one modal in the app looked like it came from a different
    // product.
    dialogTheme: DialogThemeData(
      backgroundColor: scheme.surfaceContainerLowest,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(20)),
    ),
    // The one action on the sessions screen should look like one.
    floatingActionButtonTheme: FloatingActionButtonThemeData(
      backgroundColor: scheme.primary,
      foregroundColor: scheme.onPrimary,
      elevation: 2,
    ),
    inputDecorationTheme: InputDecorationTheme(
      filled: true,
      fillColor: scheme.surfaceContainerLowest,
      contentPadding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
      border: OutlineInputBorder(
        borderRadius: BorderRadius.circular(22),
        borderSide: BorderSide(color: scheme.outlineVariant),
      ),
      enabledBorder: OutlineInputBorder(
        borderRadius: BorderRadius.circular(22),
        borderSide: BorderSide(color: scheme.outlineVariant),
      ),
      focusedBorder: OutlineInputBorder(
        borderRadius: BorderRadius.circular(22),
        borderSide: BorderSide(color: scheme.primary, width: 1.5),
      ),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(
        padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 12),
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(12)),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 12),
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(12)),
      ),
    ),
  );
}

/// A small round dot in the colour of a project's state.
///
/// Text alone ("在线" / "离线") makes the reader parse a word to learn something
/// a colour says at a glance, and the two words are the same length in Chinese.
class StatusDot extends StatelessWidget {
  const StatusDot({super.key, required this.online});
  final bool online;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      width: 8,
      height: 8,
      decoration: BoxDecoration(
        shape: BoxShape.circle,
        color: online ? const Color(0xFF2FA36B) : scheme.outline,
      ),
    );
  }
}
