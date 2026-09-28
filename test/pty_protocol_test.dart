import 'dart:async';
import 'dart:convert';

import 'package:edgecube/server/server_controller.dart';
import 'package:edgecube/server/server_service.dart';
import 'package:edgecube/shell/shell_controller.dart';
import 'package:edgecube/shell/shell_service.dart';
import 'package:edgecube/terminal/utf8_carry.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

Uint8List bytesOf(String s) => Uint8List.fromList(utf8.encode(s));

/// 把 [s] 的 UTF-8 字节**在一个多字节字符中间**切开：
/// 返回 `(该字符的首字节, 该字符剩下的字节 + 后续全部)`。
///
/// 这正是 PTY 切块 / 回放 64 KiB 分片最真实的失败模式。
(Uint8List, Uint8List) splitInsideCharacter(String s) {
  final b = bytesOf(s);
  for (var i = 0; i < b.length; i++) {
    if (b[i] >= 0x80) {
      return (
        Uint8List.sublistView(b, 0, i + 1),
        Uint8List.sublistView(b, i + 1),
      );
    }
  }
  fail('测试字符串不含多字节字符：$s');
}

/// 把 pty / tunnel 的通道都 mock 掉，并把落到 MethodChannel 上的调用记进 [calls]。
///
/// 不 mock 的话 `invokeMethod('listen')` 会抛 MissingPluginException，
/// 经 `FlutterError.reportError` 变成测试失败；`resize` 的未捕获异常同理。
void mockChannels(List<MethodCall> calls) {
  final messenger =
      TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
  for (final name in [
    'com.venti1112.edgecube/server',
    'com.venti1112.edgecube/shell',
    'com.venti1112.edgecube/tunnel_events',
  ]) {
    messenger.setMockMethodCallHandler(MethodChannel(name), (call) async {
      calls.add(call);
      return null;
    });
  }
}

void main() {
  // ——————————————————————————————————————————————————————————
  // 1. 跨帧 UTF-8 解码
  // ——————————————————————————————————————————————————————————
  group('Utf8Carry', () {
    test('中文被切成两半仍能拼回', () {
      final carry = Utf8Carry();
      final (head, tail) = splitInsideCharacter('中');
      expect(carry.decode(head), '');
      expect(carry.decode(tail), '中');
      expect(carry.hasPending, isFalse);
    });

    test('整段中文逐字节喂入仍能拼回', () {
      final carry = Utf8Carry();
      final out = StringBuffer();
      // 每次只喂 1 个字节：任意位置都可能落在字符中间。
      for (final b in bytesOf('中文终端')) {
        out.write(carry.decode(Uint8List.fromList([b])));
      }
      expect(out.toString(), '中文终端');
      expect(carry.hasPending, isFalse);
    });

    test('4 字节 emoji 跨帧', () {
      final carry = Utf8Carry();
      final bytes = bytesOf('🎉');
      expect(bytes.length, 4);
      expect(carry.decode(Uint8List.sublistView(bytes, 0, 3)), '');
      expect(carry.decode(Uint8List.sublistView(bytes, 3)), '🎉');
    });

    test('半截字符后接 ASCII：尾巴就地解成 U+FFFD，不吞后面的 ASCII', () {
      final carry = Utf8Carry();
      final (head, _) = splitInsideCharacter('中');
      expect(carry.decode(head), '');
      expect(carry.decode(bytesOf('ok')), '\uFFFDok');
      expect(carry.hasPending, isFalse);
    });

    test('非法字节就地解出，不无限缓存', () {
      final carry = Utf8Carry();
      // 0x80 是孤立续字节、0xFF 不是合法起始字节、0x41 = 'A'。
      expect(
        carry.decode(Uint8List.fromList([0x80, 0xff, 0x41])),
        '\uFFFD\uFFFDA',
      );
      expect(carry.hasPending, isFalse);
    });

    test('reset 丢掉攒着的半个字符', () {
      final carry = Utf8Carry();
      final (head, _) = splitInsideCharacter('中');
      expect(carry.decode(head), '');
      expect(carry.hasPending, isTrue);

      carry.reset();
      expect(carry.hasPending, isFalse);
      // 若没丢干净，下面的「字」会被旧前缀 E4 拼成 E4 E5 AD 97 → 前面多个 U+FFFD。
      expect(carry.decode(bytesOf('字')), '字');
    });
  });

  // ——————————————————————————————————————————————————————————
  // 2. 事件解析（回放边界 / 状态快照）
  // ——————————————————————————————————————————————————————————
  group('事件解析', () {
    test('shell: historyBegin / historyEnd 是独立事件', () {
      expect(
        parseShellEvent({'type': 'historyBegin'}),
        isA<ShellHistoryBeginEvent>(),
      );
      expect(
        parseShellEvent({'type': 'historyEnd'}),
        isA<ShellHistoryEndEvent>(),
      );
    });

    test('server: historyBegin / historyEnd 是独立事件', () {
      expect(
        parseServerEvent({'type': 'historyBegin'}),
        isA<ServerHistoryBeginEvent>(),
      );
      expect(
        parseServerEvent({'type': 'historyEnd'}),
        isA<ServerHistoryEndEvent>(),
      );
    });

    test('server: 回放态状态快照不带 exitCode（否则被当成真实退出）', () {
      final e = parseServerEvent({'type': 'state', 'status': null});
      expect(
        e,
        isA<ServerStateEvent>()
            .having((e) => e.status, 'status', isNull)
            .having((e) => e.exitCode, 'exitCode', isNull),
      );
    });

    test('server: 运行态状态快照带齐实例上下文', () {
      expect(
        parseServerEvent({
          'type': 'state',
          'status': 'running',
          'instanceId': 'i1',
          'instanceName': '生存服',
        }),
        isA<ServerStateEvent>()
            .having((e) => e.status, 'status', 'running')
            .having((e) => e.instanceId, 'instanceId', 'i1')
            .having((e) => e.instanceName, 'instanceName', '生存服')
            .having((e) => e.exitCode, 'exitCode', isNull),
      );
    });

    test('server: term 帧带原始字节、log 帧带清洗行', () {
      expect(
        parseServerEvent({'type': 'term', 'bytes': bytesOf('hi')}),
        isA<ServerTermEvent>().having((e) => e.bytes, 'bytes', bytesOf('hi')),
      );
      expect(
        parseServerEvent({'type': 'log', 'line': 'Done (5s)!'}),
        isA<ServerLogEvent>().having((e) => e.line, 'line', 'Done (5s)!'),
      );
    });

    test('回放帧序：historyBegin → term… → state → historyEnd', () {
      // 与 Kotlin `uiListener` 的 KIND_REPLAY_BEGIN / DATA / REPLAY_END 一致。
      const raw = [
        {'type': 'historyBegin'},
        {'type': 'term'},
        {'type': 'state', 'status': 'running'},
        {'type': 'historyEnd'},
      ];
      final parsed = raw.map(parseServerEvent).toList();
      expect(parsed[0], isA<ServerHistoryBeginEvent>());
      expect(parsed[1], isA<ServerTermEvent>());
      expect(parsed[2], isA<ServerStateEvent>());
      expect(parsed[3], isA<ServerHistoryEndEvent>());
    });
  });

  // ——————————————————————————————————————————————————————————
  // 3. 回放协议端到端（清屏 / 分片拼接 / 提示符只画一次 / 补发尺寸）
  // ——————————————————————————————————————————————————————————
  group('服务端回放协议', () {
    late List<MethodCall> calls;
    late StreamController<ServerEvent> events;
    late ServerController controller;

    setUp(() {
      TestWidgetsFlutterBinding.ensureInitialized();
      calls = [];
      mockChannels(calls);
      events = StreamController<ServerEvent>();
      controller = ServerController(events: events.stream);
      // 让 onResize 记下尺寸（回放结束时要原样补发）。
      controller.terminal.resize(100, 30);
      calls.clear();
    });

    tearDown(() async {
      controller.dispose();
      await events.close();
    });

    test('historyBegin 清屏、分片中文拼接、提示符只在 historyEnd 画一次',
        () async {
      // 画面里先有旧内容，用来证明 historyBegin 真的清了。
      controller.terminal.write('stale\r\n');
      expect(controller.terminal.buffer.getText(), contains('stale'));

      events.add(const ServerHistoryBeginEvent());
      await pumpEventQueue();
      expect(controller.terminal.buffer.getText(), isNot(contains('stale')));

      // 两片：第一片在「欢」字中间断开。
      final (head, tail) = splitInsideCharacter('欢迎');
      events
        ..add(ServerTermEvent(head))
        ..add(ServerTermEvent(tail))
        ..add(ServerTermEvent(bytesOf('世界\r\n')));
      await pumpEventQueue();

      // ① 跨帧 UTF-8 拼回来了；② 分片期间没把 `> ` 插进历史正文。
      expect(controller.terminal.buffer.getText(), contains('欢迎世界'));
      expect(controller.terminal.buffer.getText(), isNot(contains('> ')));

      events.add(const ServerHistoryEndEvent());
      await pumpEventQueue();
      expect(controller.terminal.buffer.getText(), contains('> '));
    });

    test('historyEnd 补发一次回放前缓存的尺寸', () async {
      events
        ..add(const ServerHistoryBeginEvent())
        ..add(const ServerHistoryEndEvent());
      await pumpEventQueue();

      final resize = calls.where((c) => c.method == 'resize').lastOrNull;
      expect(resize, isNotNull, reason: 'historyEnd 应回发一次 resize');
      final args = resize!.arguments as Map;
      expect(args['cols'], 100);
      expect(args['rows'], 30);
    });

    test('回放前没缓存过尺寸时不发 resize（引擎刚重建、还没布局）', () async {
      final freshEvents = StreamController<ServerEvent>();
      final fresh = ServerController(events: freshEvents.stream);
      calls.clear();

      freshEvents
        ..add(const ServerHistoryBeginEvent())
        ..add(const ServerHistoryEndEvent());
      await pumpEventQueue();

      expect(calls.where((c) => c.method == 'resize'), isEmpty);
      fresh.dispose();
      await freshEvents.close();
    });
  });

  group('Shell 回放协议', () {
    late List<MethodCall> calls;
    late StreamController<ShellEvent> events;
    late ShellController controller;

    setUp(() {
      TestWidgetsFlutterBinding.ensureInitialized();
      calls = [];
      mockChannels(calls);
      events = StreamController<ShellEvent>();
      controller = ShellController(events: events.stream);
    });

    tearDown(() async {
      controller.dispose();
      await events.close();
    });

    test('historyBegin 清屏后分片历史原样铺开', () async {
      controller.terminal.write('old\r\n');
      expect(controller.terminal.buffer.getText(), contains('old'));

      events.add(const ShellHistoryBeginEvent());
      await pumpEventQueue();
      expect(controller.terminal.buffer.getText(), isNot(contains('old')));

      final (head, tail) = splitInsideCharacter('中文');
      events
        ..add(ShellTermEvent(head))
        ..add(ShellTermEvent(tail))
        ..add(ShellTermEvent(bytesOf(r'$ /system/bin/sh' '\r\n')));
      await pumpEventQueue();

      expect(controller.terminal.buffer.getText(), contains('中文'));
      expect(
        controller.terminal.buffer.getText(),
        contains(r'$ /system/bin/sh'),
      );
    });

    test('historyEnd 补发一次回放前缓存的尺寸', () async {
      controller.terminal.resize(120, 40);
      calls.clear();

      events
        ..add(const ShellHistoryBeginEvent())
        ..add(const ShellHistoryEndEvent());
      await pumpEventQueue();

      final resize = calls.where((c) => c.method == 'resize').lastOrNull;
      expect(resize, isNotNull, reason: 'historyEnd 应回发一次 resize');
      final args = resize!.arguments as Map;
      expect(args['cols'], 120);
      expect(args['rows'], 40);
    });
  });
}
