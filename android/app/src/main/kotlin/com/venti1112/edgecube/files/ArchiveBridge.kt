package com.venti1112.edgecube.files

/**
 * 归档进度回调。
 */
fun interface ArchiveProgressListener {
    fun onProgress(current: Int, total: Int)
}

/**
 * archive 功能 JNI 桥：解压 / 压缩 / ecpkg 提取由 Rust 实现
 */
internal object ArchiveBridge {

    init {
        System.loadLibrary("edgecube_tools")
    }

    /** 对应 `ArchiveExtractor.compressToZip`：返回写入的文件数。 */
    external fun nativeCompressToZip(sources: Array<String>, archivePath: String): Int

    /** 对应 `ArchiveExtractor.extract`：返回解压出的文件数。 */
    external fun nativeExtract(
        archivePath: String,
        destDir: String,
        listener: ArchiveProgressListener?,
    ): Int

    /** 读取单条目内容；缺失抛 `IllegalArgumentException("ZIP 中缺少 <name>")`。 */
    external fun nativeZipReadEntry(archivePath: String, entryName: String): String

    /** ZIP 中是否存在 `dirName/` 前缀条目。 */
    external fun nativeZipHasDirPrefix(archivePath: String, dirName: String): Boolean

    /**
     * ecpkg 双前缀提取（universalDir 先、archDir 后覆盖），
     * 返回 `[processed, total]`；条目逃逸抛 `SecurityException`。
     */
    external fun nativeEcpkgExtract(
        archivePath: String,
        destDir: String,
        universalDir: String?,
        archDir: String,
    ): IntArray
}
