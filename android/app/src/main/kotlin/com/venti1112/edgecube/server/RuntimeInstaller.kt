package com.venti1112.edgecube.server

import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.system.Os
import android.system.OsConstants
import com.venti1112.edgecube.files.ArchiveBridge
import java.io.File

/**
 * 运行时管理器
 */
object RuntimeInstaller {

    fun runtimesDir(context: Context): File = File(context.filesDir, "runtimes")

    fun runtimeDir(context: Context, id: String): File =
        File(runtimesDir(context), id)

    /** 扫描所有已安装且有效的运行时。 */
    fun installedRuntimes(context: Context): List<EcManifest> {
        val dir = runtimesDir(context)
        if (!dir.isDirectory) return emptyList()
        val result = mutableListOf<EcManifest>()
        for (child in dir.listFiles() ?: emptyArray()) {
            if (!child.isDirectory || child.name.startsWith(".")) continue
            val manifest = readInstalledManifest(child) ?: continue
            result.add(manifest)
        }
        return result
    }

    /** 读取指定 id 的已安装运行时清单；未安装或无效返回 null。 */
    fun installedRuntime(context: Context, id: String): EcManifest? {
        val dir = runtimeDir(context, id)
        if (!dir.isDirectory) return null
        return readInstalledManifest(dir)
    }

    /** 指定运行时是否已安装且有效。 */
    fun isInstalled(context: Context, id: String): Boolean {
        return installedRuntime(context, id) != null
    }

    /** 当前已安装的 JRE 运行时 id 列表。 */
    fun availableJreIds(context: Context): List<String> {
        return installedRuntimes(context)
            .filter { it.type == "jre" }
            .map { it.id }
    }

    /** 当前已安装的 PHP 运行时 id 列表。 */
    fun availablePhpIds(context: Context): List<String> {
        val installed = installedRuntimes(context)
            .filter { it.type == "php" }
            .map { it.id }
            .toMutableList()
        // PHP CLI 随 APK 内置（libphp-cli.so），总是可用。放在列表首位，
        // 让 UI 默认选中内置版本。
        if (!installed.contains(BUILTIN_PHP_CLI_ID)) {
            installed.add(0, BUILTIN_PHP_CLI_ID)
        }
        return installed
    }

    /** 内置 PHP CLI 的运行时 id（随 APK 打包的 musl 静态链接 PHP 8.2）。 */
    const val BUILTIN_PHP_CLI_ID = "php-cli-8.2"

    /** 取首个已安装的 frpc 运行时（用于隧道）。 */
    fun installedFrpc(context: Context): EcManifest? {
        return installedRuntimes(context).firstOrNull { it.type == "frpc" }
    }

    /** 是否存在已安装的 frpc 运行时。 */
    fun isFrpcAvailable(context: Context): Boolean = installedFrpc(context) != null

    /**
     * 导入 `.ecpkg` 文件并安装到 `runtimes/<id>/`。
     *
     * @param force 为 true 时不询问直接覆盖已存在的同 id 运行时。
     */
    fun importPackage(
        context: Context,
        ecpkgPath: String,
        onProgress: ((Int, Int) -> Unit)? = null,
        force: Boolean = false,
    ): EcManifest {
        val file = File(ecpkgPath)
        if (!file.isFile) throw IllegalArgumentException("文件不存在：$ecpkgPath")

        // 1. 读取并解析清单
        val manifestJson = ArchiveBridge.nativeZipReadEntry(ecpkgPath, "edgecube-package.json")
        val manifest = EcPackage.parse(manifestJson)

        // 2. 校验 id
        if (!EcPackage.validateId(manifest.id)) {
            throw IllegalArgumentException("运行时 id 包含非法字符：${manifest.id}")
        }

        // 3. 校验设备架构
        val archDir = EcPackage.pickArchDir(context, manifest)
            ?: throw IllegalArgumentException("当前设备架构不支持此包")

        // 4. 校验 minAppVersion
        val minVer = manifest.minAppVersion
        val appVer = try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                context.packageManager.getPackageInfo(context.packageName, PackageManager.GET_ACTIVITIES).longVersionCode
            } else {
                @Suppress("DEPRECATION")
                context.packageManager.getPackageInfo(context.packageName, 0).versionCode.toLong()
            }
        } catch (_: Exception) { 0L }
        if (appVer < minVer) {
            throw IllegalArgumentException("应用版本过低，需要构建号 ≥ $minVer")
        }

        // 5. 校验 universalDir / archDir 在 ZIP 中存在
        manifest.universalDir?.let { ud ->
            if (!ArchiveBridge.nativeZipHasDirPrefix(ecpkgPath, ud)) {
                throw IllegalArgumentException("ZIP 中缺少 universalDir：$ud")
            }
        }
        if (!ArchiveBridge.nativeZipHasDirPrefix(ecpkgPath, archDir)) {
            throw IllegalArgumentException("ZIP 中缺少 archDir：$archDir")
        }

        // 6. 安全替换：解压到临时目录
        val target = runtimeDir(context, manifest.id)
        val tmpDir = File(runtimesDir(context), ".${manifest.id}.tmp")
        if (target.exists() && !force) {
            throw IllegalStateException("RUNTIME_EXISTS")
        }
        tmpDir.deleteRecursively()
        tmpDir.mkdirs()

        try {
            // 双前缀提取（universal 先、arch 覆盖）；路径逃逸抛 SecurityException。
            val counts = ArchiveBridge.nativeEcpkgExtract(
                ecpkgPath,
                tmpDir.absolutePath,
                manifest.universalDir,
                archDir,
            )
            val processed = counts[0]
            val total = counts[1]

            // 设置可执行位
            setExecutableBits(tmpDir)

            // SONAME 兜底链接
            createSonameLinks(tmpDir)

            // 复制清单
            File(tmpDir, "edgecube-package.json").writeText(manifestJson)

            // 最后写 version（作为完成标记）
            File(tmpDir, "version").writeText(manifest.version.toString())

            onProgress?.invoke(processed, total)

            // 原子替换
            target.deleteRecursively()
            if (!tmpDir.renameTo(target)) {
                throw IllegalStateException("无法重命名临时目录到目标目录")
            }
        } catch (e: Throwable) {
            tmpDir.deleteRecursively()
            throw e
        }

        return manifest
    }

    /** 删除指定运行时。 */
    fun deleteRuntime(context: Context, id: String) {
        runtimeDir(context, id).deleteRecursively()
    }

    // 内部辅助

    private fun readInstalledManifest(dir: File): EcManifest? {
        val manifestFile = File(dir, "edgecube-package.json")
        val versionFile = File(dir, "version")
        if (!manifestFile.isFile || !versionFile.isFile) return null
        return try {
            val manifest = EcPackage.parse(manifestFile.readText())
            val installedVersion = versionFile.readText().trim()
            if (installedVersion == manifest.version.toString()) manifest else null
        } catch (_: Exception) {
            null
        }
    }

    // 对 *.so 和 bin/* 设置可执行位。
    private fun setExecutableBits(root: File) {
        root.walkTopDown().forEach { f ->
            if (f.isFile) {
                val name = f.name
                val parent = f.parentFile?.name ?: ""
                if (name.endsWith(".so") || parent == "bin" || name.endsWith(".exe")) {
                    f.setExecutable(true, false)
                }
            }
        }
    }

    // 为 lib/ 下带版本号的 .so 创建 SONAME 符号链接兜底。
    private fun createSonameLinks(root: File) {
        val libDir = File(root, "lib")
        if (!libDir.isDirectory) return
        for (f in libDir.listFiles() ?: emptyArray()) {
            val name = f.name
            val soIdx = name.indexOf(".so.")
            if (soIdx < 0 || !f.isFile || isSymlink(f)) continue
            val base = name.substring(0, soIdx + 3) // "libfoo.so"
            val versionPart = name.substring(soIdx + 4) // "16.1.1"
            val parts = versionPart.split(".")
            var linkName = base
            for (i in 0 until parts.size - 1) {
                linkName = "$linkName.${parts[i]}"
                val linkFile = File(libDir, linkName)
                if (!linkFile.exists()) {
                    try { Os.symlink(name, linkFile.absolutePath) } catch (_: Throwable) {}
                }
            }
            val baseFile = File(libDir, base)
            if (!baseFile.exists() && parts.size > 1) {
                try { Os.symlink(name, baseFile.absolutePath) } catch (_: Throwable) {}
            }
        }
    }

    /**
     * 判断文件是否为符号链接。
     */
    private fun isSymlink(f: File): Boolean {
        return try {
            OsConstants.S_ISLNK(Os.lstat(f.absolutePath).st_mode)
        } catch (_: Throwable) {
            false
        }
    }
}
