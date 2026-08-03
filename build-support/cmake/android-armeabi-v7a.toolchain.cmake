# Android armeabi-v7a toolchain wrapper for cmake-rs cross-compiles.
#
# See `android-arm64-v8a.toolchain.cmake` for the rationale. Same fix, just
# the 32-bit ARMv7 ABI.

set(ANDROID_ABI armeabi-v7a)
# HTC Desire C uses a Cortex-A5 variant without a guaranteed NEON unit.
# Keep the 32-bit build on the ARMv7 baseline so it runs on every
# armeabi-v7a device, not only NEON-capable phones.
set(ANDROID_ARM_NEON FALSE)
# The MSM7225A/Cortex-A5 in HTC Desire C has VFPv3 but no NEON. NDK
# defaults for armeabi-v7a are allowed to enable NEON, so force the Unicorn
# C sources to the ARMv7 + VFP baseline explicitly.
set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -march=armv7-a -mfpu=vfpv3-d16 -mfloat-abi=softfp")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -march=armv7-a -mfpu=vfpv3-d16 -mfloat-abi=softfp")
if(NOT DEFINED ANDROID_PLATFORM)
    set(ANDROID_PLATFORM android-16)
endif()

if(NOT DEFINED ENV{ANDROID_NDK_HOME} AND NOT DEFINED ENV{ANDROID_NDK_ROOT})
    message(FATAL_ERROR
        "Set ANDROID_NDK_HOME (or ANDROID_NDK_ROOT) before invoking cargo. "
        "Required for the NDK CMake toolchain include below.")
endif()

if(DEFINED ENV{ANDROID_NDK_HOME})
    set(_pockethle_ndk_path "$ENV{ANDROID_NDK_HOME}")
else()
    set(_pockethle_ndk_path "$ENV{ANDROID_NDK_ROOT}")
endif()

include("${_pockethle_ndk_path}/build/cmake/android.toolchain.cmake")
