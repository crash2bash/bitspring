# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile
# Classes Rust reaches over JNI (src-tauri/src/android_media.rs). R8 sees no Kotlin caller for
# them, so without these rules a release build drops PlaybackService.update/progress/clear and
# Updater's methods (NoSuchMethodError: no media notification, no lock-screen controls) and
# renames the native callbacks the Rust side exports as Java_dev_local_hifi_*.
-keep class dev.local.hifi.PlaybackService { *; }
-keep class dev.local.hifi.PlaybackService$Companion { *; }
-keep class dev.local.hifi.Updater { *; }
-keep class dev.local.hifi.MainActivity { *; }
-keep class dev.local.hifi.MainActivity$Companion { *; }
-keepclasseswithmembernames,includedescriptorclasses class * {
    native <methods>;
}
