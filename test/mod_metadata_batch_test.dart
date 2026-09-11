import 'dart:convert';
import 'dart:io';

import 'package:archive/archive.dart';
import 'package:edgecube/mods/mod_metadata.dart';
import 'package:edgecube/mods/modrinth_service.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:path/path.dart' as p;

/// 写一个只含 fabric.mod.json 的最小 .jar，用于验证批量解析/批量哈希。
Future<File> _writeFakeJar(Directory dir, int index) async {
  final metadata = utf8.encode(
    '{"id":"mod$index","name":"Mod $index","version":"1.$index.0"}',
  );
  final archive = Archive()
    ..addFile(
      ArchiveFile('fabric.mod.json', metadata.length, metadata),
    );
  final file = File(p.join(dir.path, 'mod$index.jar'));
  await file.writeAsBytes(ZipEncoder().encode(archive));
  return file;
}

void main() {
  late Directory tempDir;
  late List<File> jars;

  setUp(() async {
    tempDir = await Directory.systemTemp.createTemp('edgecube_mod_test');
    jars = [for (var i = 0; i < 12; i++) await _writeFakeJar(tempDir, i)];
  });

  tearDown(() async {
    if (await tempDir.exists()) await tempDir.delete(recursive: true);
  });

  test('parseAll 批量解析结果与逐个 parse 一致', () async {
    final batch = await ModMetadataParser.parseAll(
      [for (final jar in jars) jar.path],
    );

    expect(batch, hasLength(jars.length));
    for (var i = 0; i < jars.length; i++) {
      final single = await ModMetadataParser.parse(jars[i].path);
      expect(single, isNotNull);
      expect(batch[jars[i].path]?.name, single!.name);
      expect(batch[jars[i].path]?.name, 'Mod $i');
      expect(batch[jars[i].path]?.version, '1.$i.0');
    }
  });

  test('parseAll 对空列表直接返回空结果', () async {
    expect(await ModMetadataParser.parseAll(const []), isEmpty);
    expect(await ModrinthService.computeSha1Batch(const []), isEmpty);
  });

  test('computeSha1Batch 与逐个 computeSha1 结果一致', () async {
    final paths = [for (final jar in jars) jar.path];
    final batch = await ModrinthService.computeSha1Batch(paths);

    expect(batch, hasLength(paths.length));
    for (final path in paths) {
      final single = await ModrinthService.computeSha1(path);
      expect(batch[path], single);
      expect(batch[path], hasLength(40));
    }
  });

  test('读取失败的文件在批量结果里为空串而不是抛异常', () async {
    final missing = p.join(tempDir.path, 'not_exists.jar');
    final hashes = await ModrinthService.computeSha1Batch([missing]);

    expect(hashes[missing], '');
    final parsed = await ModMetadataParser.parseAll([missing]);
    expect(parsed[missing], isNull);
  });

  test('批量往返的 isolate 开销远小于逐个文件', () async {
    final paths = [for (final jar in jars) jar.path];

    final perFile = Stopwatch()..start();
    for (final path in paths) {
      await ModMetadataParser.parse(path);
      await ModrinthService.computeSha1(path);
    }
    perFile.stop();

    final batch = Stopwatch()..start();
    await ModMetadataParser.parseAll(paths);
    await ModrinthService.computeSha1Batch(paths);
    batch.stop();

    // 逐个文件 = 2×N 次 isolate 往返，批量 = 2 次；样本太小时计时噪声大，
    // 只在样本足够（逐个明显不是 0ms）时才做方向性断言。
    if (perFile.elapsedMilliseconds >= 20) {
      expect(batch.elapsedMilliseconds, lessThan(perFile.elapsedMilliseconds));
    }
  });
}
