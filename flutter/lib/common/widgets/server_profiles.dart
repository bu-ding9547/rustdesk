import 'dart:convert';

import 'package:flutter/material.dart';

import '../../common.dart';
import '../../models/platform_model.dart';

/// Local option holding the user's saved server configurations.
///
/// Stored per device (local config, not synced to the server) because the same
/// account is often reached through a different relay depending on the network.
const kServerProfilesOption = 'server-profiles';

/// One named server configuration the user can switch to.
class ServerProfile {
  String name;
  ServerConfig config;

  ServerProfile({required this.name, required this.config});

  Map<String, dynamic> toJson() => {
        'name': name,
        'id': config.idServer,
        'relay': config.relayServer,
        'api': config.apiServer,
        'key': config.key,
      };

  static ServerProfile? fromJson(dynamic value) {
    if (value is! Map) return null;
    final name = value['name']?.toString().trim() ?? '';
    if (name.isEmpty) return null;
    return ServerProfile(
      name: name,
      config: ServerConfig(
        idServer: value['id']?.toString(),
        relayServer: value['relay']?.toString(),
        apiServer: value['api']?.toString(),
        key: value['key']?.toString(),
      ),
    );
  }

  bool matches(ServerConfig other) =>
      config.idServer == other.idServer &&
      config.relayServer == other.relayServer &&
      config.apiServer == other.apiServer &&
      config.key == other.key;
}

class ServerProfiles {
  /// Reads the saved profiles, dropping entries that are no longer decodable.
  static List<ServerProfile> load() {
    final raw = bind.mainGetLocalOption(key: kServerProfilesOption);
    if (raw.isEmpty) return [];
    try {
      final decoded = jsonDecode(raw);
      if (decoded is! List) return [];
      return decoded.map(ServerProfile.fromJson).whereType<ServerProfile>().toList();
    } catch (e) {
      debugPrint('Invalid server profiles: $e');
      return [];
    }
  }

  static Future<void> save(List<ServerProfile> profiles) =>
      bind.mainSetLocalOption(
          key: kServerProfilesOption,
          value: jsonEncode(profiles.map((e) => e.toJson()).toList()));

  /// Adds [profile], replacing the entry that carries the same name.
  static Future<void> upsert(
      List<ServerProfile> profiles, ServerProfile profile) async {
    profiles.removeWhere((e) => e.name == profile.name);
    profiles.add(profile);
    await save(profiles);
  }

  static Future<void> remove(
      List<ServerProfile> profiles, ServerProfile profile) async {
    profiles.removeWhere((e) => e.name == profile.name);
    await save(profiles);
  }
}
