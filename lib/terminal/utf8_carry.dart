import 'dart:convert';
import 'dart:typed_data';

/// 跨帧的 UTF-8 流式解码器。
///
/// PTY 输出按「读到多少算多少」切块，**切点可以落在一个多字节字符中间**：
/// 单独 `utf8.decode(chunk, allowMalformed: true)` 会把半个汉字解成 U+FFFD，
/// 下一块剩下的字节又各是一个 U+FFFD —— 中文日志直接烂掉。
///
/// 回放历史时切点是 64 KiB 的分片边界（见 Rust `REPLAY_CHUNK_BYTES`），
/// 更是与字符边界毫无关系，所以这里必须把尾巴攒住。
///
/// 只攒**前缀合法但还没收齐**的尾部字节（≤ 3 字节；4 字节序列收齐即出）；
/// 其余（含非法字节）一律交给 `allowMalformed` 就地解出，绝不无限缓存。
class Utf8Carry {
  final BytesBuilder _pending = BytesBuilder();

  /// 解码 [bytes]，返回其中**完整**的字符；不足一个字符的尾巴攒在内部。
  String decode(Uint8List bytes) {
    if (bytes.isEmpty && _pending.length == 0) return '';
    _pending.add(bytes);
    final all = _pending.takeBytes();
    final tail = _incompleteTailLength(all);
    if (tail == all.length) {
      // 整段都还是半个字符：放回内部，等下一块。
      _pending.add(all);
      return '';
    }
    final cut = all.length - tail;
    if (tail > 0) _pending.add(Uint8List.sublistView(all, 0, cut));
    return utf8.decode(
      tail > 0 ? Uint8List.sublistView(all, 0, cut) : all,
      allowMalformed: true,
    );
  }

  /// 丢掉未解出的尾巴（历史回放开始时调：前面攒的字属于被清掉的画面）。
  void reset() => _pending.clear();

  /// 是否还攒着半个字符。
  bool get hasPending => _pending.length > 0;

  /// 返回 [d] 末尾「合法但尚未收齐」的字节数；没有则为 0。
  static int _incompleteTailLength(Uint8List d) {
    final n = d.length;
    // UTF-8 最长 4 字节，最多往回看 4 个字节即可。
    final start = n > 4 ? n - 4 : 0;
    for (var i = n - 1; i >= start; i--) {
      final b = d[i];
      if (b < 0x80) return 0; // ASCII：它前面一定都是完整的
      if ((b & 0xC0) == 0xC0) {
        // 起始字节：need = 序列总长，have = 到结尾为止已有的字节数。
        final need = b < 0xE0 ? 2 : b < 0xF0 ? 3 : b < 0xF8 ? 4 : 0;
        if (need == 0) return 0; // 非法起始字节，交给 allowMalformed
        final have = n - i;
        return have < need ? have : 0;
      }
      // 0x80..0xBF：续字节，继续往回看。
    }
    // 回看了 4 个字节全是续字节（非法），交给 allowMalformed。
    return 0;
  }
}
