package com.venti1112.edgecube.files

/**
 * 归档解压器
 *
 * 支持格式：zip、tar、tar.gz/tgz、tar.xz/txz、tar.bz2/tbz2、tar.zst、tar.lz4、
 * 7z、rar，以及单文件压缩流 xz / gz / bz2 / zst / lz4。
 * 压缩格式：zip。
 */
object ArchiveExtractor {

    /**
     * 把 [sourcePaths] 中的文件/目录压缩为 zip 文件 [archivePath]。
     */
    fun compressToZip(sourcePaths: List<String>, archivePath: String): Int {
        if (sourcePaths.isEmpty()) {
            throw IllegalArgumentException("没有可压缩的文件")
        }
        return ArchiveBridge.nativeCompressToZip(sourcePaths.toTypedArray(), archivePath)
    }

    /**
     * 解压 [archivePath] 到 [destDir]。
     *
     * @param archivePath 归档文件绝对路径。
     * @param destDir 目标目录（已存在）。
     * @param onProgress 可选的进度回调，参数为 (current, total)。
     * @return 解压出的文件数量。
     */
    fun extract(
        archivePath: String,
        destDir: String,
        onProgress: ((current: Int, total: Int) -> Unit)? = null,
    ): Int {
        val listener = onProgress?.let { notify ->
            ArchiveProgressListener { current, total -> notify(current, total) }
        }
        return ArchiveBridge.nativeExtract(archivePath, destDir, listener)
    }
}
