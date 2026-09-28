cmake_minimum_required(VERSION 3.18.1)

project("edgecube_launch" C)

set(CMAKE_C_STANDARD 11)

add_executable(launch launcher.c)

set_target_properties(launch PROPERTIES
        PREFIX "lib"
        SUFFIX ".so"
        OUTPUT_NAME "launch")

target_compile_options(launch PRIVATE -fPIE)
target_link_options(launch PRIVATE -fPIE -pie)

# dl：dlopen/dlsym；log：__android_log_print（方便用 logcat 看启动器日志）。
target_link_libraries(launch dl log)

# LD_PRELOAD 标签修复库：拦截 malloc/free 以恢复 Android 堆指针标签
add_library(tagfix SHARED tagfix.c)
target_link_libraries(tagfix dl log)

# ─── libfrpcloader.so ─── frpc 启动器（dlopen 数据目录里的 libfrpc.so）
add_executable(frpcloader frpcloader.c)

set_target_properties(frpcloader PROPERTIES
        PREFIX "lib"
        SUFFIX ".so"
        OUTPUT_NAME "frpcloader")

target_compile_options(frpcloader PRIVATE -fPIE)
target_link_options(frpcloader PRIVATE -fPIE -pie)
target_link_libraries(frpcloader dl log)
