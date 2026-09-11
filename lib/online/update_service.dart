import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:http/http.dart' as http;
import 'package:package_info_plus/package_info_plus.dart';
import 'package:path/path.dart' as p;
import 'package:path_provider/path_provider.dart';
import 'package:flutter/services.dart';

import '../config/network_store.dart';
import '../net/download_engine.dart';
import 'cloud_headers.dart';

class DownloadLink {
  const DownloadLink({
    required this.name,
    required this.url,
    required this.type,
    required this.extra,
  });

  final String name;
  final String url;
  final String type;
  final String extra;

  bool get isDirect => type == 'direct';

  bool get isWebPage => type == 'web';

  factory DownloadLink.fromJson(Map<String, dynamic> json) => DownloadLink(
    name: json['name'] as String,
    url: json['url'] as String,
    type: json['type'] as String,
    extra: json['extra'] as String? ?? '',
  );
}

class AppUpdateInfo {
  const AppUpdateInfo({
    required this.version,
    required this.build,
    required this.sha256,
    required this.releaseNotes,
    required this.downloadLinks,
  });

  final String version;
  final int build;
  final String sha256;
  final String releaseNotes;
  final List<DownloadLink> downloadLinks;

  List<DownloadLink> get directLinks =>
      downloadLinks.where((l) => l.isDirect).toList();

  DownloadLink? get firstDirectLink =>
      directLinks.isEmpty ? null : directLinks.first;

  factory AppUpdateInfo.fromJson(Map<String, dynamic> json) => AppUpdateInfo(
    version: json['version'] as String,
    build: json['build'] as int,
    sha256: json['sha256'] as String,
    releaseNotes: json['releaseNotes'] as String,
    downloadLinks: (json['download_links'] as List<dynamic>)
        .map((e) => DownloadLink.fromJson(e as Map<String, dynamic>))
        .toList(),
  );
}

class UpdateService {
  UpdateService._();

  static const _channel = MethodChannel('com.venti1112.edgecube/update');

  static const _githubRepoUrl =
      'https://api.github.com/repos/venti1112/EdgeCube/releases';
  static const _giteeRepoUrl =
      'https://gitee.com/api/v5/repos/venti1112/EdgeCube/releases';

  static Future<int> getCurrentBuild() async {
    final info = await PackageInfo.fromPlatform();
    return int.tryParse(info.buildNumber) ?? 0;
  }

  static bool hasUpdate(AppUpdateInfo channelInfo, int currentBuild) {
    return channelInfo.build > currentBuild;
  }

  static Future<AppUpdateInfo?> pickBestUpdate(AppUpdateInfo result) async {
    final currentBuild = await getCurrentBuild();
    final enableBeta = await NetworkStore.loadBetaUpdates();

    if (!enableBeta) {
      final tag = result.version.toLowerCase();
      if (tag.contains('beta') || tag.contains('alpha') || tag.contains('rc')) {
        return null;
      }
    }

    return hasUpdate(result, currentBuild) ? result : null;
  }

  /// 从 GitHub/Gitee 并行获取最新更新信息，哪个先返回有效结果就用哪个。
  static Future<AppUpdateInfo?> checkForUpdates() async {
    final completer = Completer<AppUpdateInfo?>();
    var failures = 0;
    const totalSources = 2;

    try {
      final info = await PackageInfo.fromPlatform();
      final headers = await CloudHeaders.base();
      headers['X-App-Version'] = info.version;
      headers['X-App-Build'] = info.buildNumber;

      Future<void> fetchFromSource(
        String url,
        Map<String, String> requestHeaders,
        String sourceName,
      ) async {
        try {
          final response = await http
              .get(Uri.parse(url), headers: requestHeaders)
              .timeout(const Duration(seconds: 15));
          if (response.statusCode != 200) {
            throw Exception('HTTP ${response.statusCode}');
          }
          final body = utf8.decode(response.bodyBytes);
          final releases = jsonDecode(body) as List<dynamic>;
          final result = _parseReleases(releases, sourceName);
          if (result != null && !completer.isCompleted) {
            completer.complete(result);
          }
        } catch (_) {
          if (!completer.isCompleted) {
            failures++;
            if (failures == totalSources) {
              completer.complete(null);
            }
          }
        }
      }

      fetchFromSource(_githubRepoUrl, headers, 'GitHub');
      fetchFromSource(_giteeRepoUrl, {}, 'Gitee');
    } catch (_) {
      if (!completer.isCompleted) {
        completer.complete(null);
      }
    }

    return completer.future;
  }

  /// 从 releases 列表解析出最新的有效更新信息。
  static AppUpdateInfo? _parseReleases(
    List<dynamic> releases,
    String sourceName,
  ) {
    for (final release in releases) {
      final info = _parseRelease(release as Map<String, dynamic>, sourceName);
      if (info != null) return info;
    }
    return null;
  }

  /// 从单个 release 解析更新信息，无效时返回 null。
  static AppUpdateInfo? _parseRelease(
    Map<String, dynamic> release,
    String sourceName,
  ) {
    final tagName = release['tag_name'] as String?;
    if (tagName == null || tagName.isEmpty) return null;

    final assets = release['assets'] as List<dynamic>?;
    if (assets == null || assets.isEmpty) return null;

    String? apkUrl;
    String sha256Hash = '';
    for (final asset in assets) {
      final assetMap = asset as Map<String, dynamic>;
      final downloadUrl = assetMap['browser_download_url'] as String?;
      if (downloadUrl != null && downloadUrl.toLowerCase().endsWith('.apk')) {
        apkUrl = downloadUrl;
        final digest = assetMap['digest'] as String?;
        if (digest != null && digest.startsWith('sha256:')) {
          sha256Hash = digest.substring(7);
        }
        break;
      }
    }
    if (apkUrl == null) return null;

    final version = _parseVersionFromTag(tagName);
    final build = _parseBuildFromTag(tagName);
    if (version == null || build == null) return null;

    final body = release['body'] as String? ?? '';

    return _buildAppUpdateInfo(
      version: version,
      build: build,
      sha256: sha256Hash,
      releaseNotes: body,
      primarySource: sourceName,
      primaryUrl: apkUrl,
    );
  }

  /// 从 tag_name 解析版本号（去掉 v 前缀）。
  static String? _parseVersionFromTag(String tagName) {
    if (tagName.isEmpty) return null;
    var version = tagName;
    if (version.toLowerCase().startsWith('v')) {
      version = version.substring(1);
    }
    return version.isEmpty ? null : version;
  }

  /// 从 tag_name 解析 build 号（提取末尾数字）。
  static int? _parseBuildFromTag(String tagName) {
    final match = RegExp(r'(\d+)\s*$').firstMatch(tagName);
    if (match == null) return null;
    return int.tryParse(match.group(1)!);
  }

  /// 构造 AppUpdateInfo，包含两个下载源（Gitee 默认在前）。
  static AppUpdateInfo _buildAppUpdateInfo({
    required String version,
    required int build,
    required String sha256,
    required String releaseNotes,
    required String primarySource,
    required String primaryUrl,
  }) {
    final giteeUrl =
        'https://gitee.com/venti1112/EdgeCube/releases/download/$version/EdgeCube-$version.apk';
    final githubUrl =
        'https://github.com/venti1112/EdgeCube/releases/download/v$version/EdgeCube-v$version.apk';

    final List<DownloadLink> downloadLinks;
    if (primarySource == 'GitHub') {
      downloadLinks = [
        DownloadLink(name: 'Gitee', url: giteeUrl, type: 'direct', extra: ''),
        DownloadLink(
          name: 'GitHub',
          url: primaryUrl,
          type: 'direct',
          extra: '',
        ),
      ];
    } else {
      downloadLinks = [
        DownloadLink(
          name: 'Gitee',
          url: primaryUrl,
          type: 'direct',
          extra: '',
        ),
        DownloadLink(name: 'GitHub', url: githubUrl, type: 'direct', extra: ''),
      ];
    }

    return AppUpdateInfo(
      version: version,
      build: build,
      sha256: sha256,
      releaseNotes: releaseNotes,
      downloadLinks: downloadLinks,
    );
  }

  /// 下载 APK（单源），委托全局下载引擎（分片并行 + 断点续传）。
  static Future<String> downloadApk(
    String url, {
    void Function(DownloadProgress progress)? onProgress,
  }) async {
    return downloadApkMultiSource([url], onProgress: onProgress);
  }

  /// 下载 APK，按顺序在多个直链间回退；可选 [sha256] 由引擎内校验。
  /// 返回下载到临时目录的文件路径。
  static Future<String> downloadApkMultiSource(
    List<String> urls, {
    void Function(DownloadProgress progress)? onProgress,
    String? sha256,
  }) async {
    final first = urls.firstWhere((u) => u.isNotEmpty, orElse: () => '');
    final cacheDir = await getTemporaryDirectory();
    final fileName = _extractFileName(first);
    final filePath = p.join(cacheDir.path, fileName);
    await DownloadEngine.instance.downloadToFileMultiSource(
      urls,
      filePath,
      sha256: sha256,
      onProgress: onProgress,
    );
    return filePath;
  }

  static Future<bool> verifySha256(
    String filePath,
    String expectedSha256,
  ) async {
    try {
      if (expectedSha256.isEmpty) return true;
      final file = File(filePath);
      final bytes = await file.readAsBytes();
      final digest = sha256.convert(bytes);
      return digest.toString().toLowerCase() == expectedSha256.toLowerCase();
    } catch (_) {
      return false;
    }
  }

  static Future<bool> verifyApkSignature(String apkPath) async {
    try {
      final result = await _channel.invokeMethod<bool>('verifySignature', {
        'apkPath': apkPath,
      });
      return result ?? false;
    } catch (_) {
      return false;
    }
  }

  static Future<void> installApk(String apkPath) async {
    await _channel.invokeMethod<void>('installApk', {'apkPath': apkPath});
  }

  static String _extractFileName(String url) {
    try {
      final uri = Uri.parse(url);
      final name = uri.pathSegments.last;
      if (name.isNotEmpty && name.toLowerCase().endsWith('.apk')) return name;
    } catch (_) {}
    return 'edgecube_update.apk';
  }
}
