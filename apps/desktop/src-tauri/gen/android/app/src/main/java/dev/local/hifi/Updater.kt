package dev.local.hifi

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import java.io.File

/**
 * Hands a downloaded update to Android's package installer (called from Rust, update_on.rs).
 * Android shows its own confirmation, and accepts the APK only if it is signed with the same
 * key as the installed app.
 */
object Updater {
  /** False when this app isn't allowed to install apps yet: the settings page for that opens. */
  @JvmStatic
  fun install(ctx: Context, path: String): Boolean {
    if (Build.VERSION.SDK_INT >= 26 && !ctx.packageManager.canRequestPackageInstalls()) {
      ctx.startActivity(
        Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${ctx.packageName}"))
          .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      )
      return false
    }
    val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.fileprovider", File(path))
    ctx.startActivity(
      Intent(Intent.ACTION_VIEW)
        .setDataAndType(uri, "application/vnd.android.package-archive")
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
    )
    return true
  }
}
