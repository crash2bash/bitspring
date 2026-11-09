package dev.local.hifi

import android.Manifest
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.View
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  companion object {
    /** Rust (android_media.rs): storage access was just granted — scan the Music folder. */
    @JvmStatic external fun nativeStorageGranted()
    private const val REQUEST_STORAGE = 1
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // Light status/navigation icons over the app's dark panel colour.
    enableEdgeToEdge(SystemBarStyle.dark(Color.TRANSPARENT), SystemBarStyle.dark(Color.TRANSPARENT))
    super.onCreate(savedInstanceState)
    keepClearOfSystemBars()
    requestAccess()
  }

  /** Android draws edge to edge, but its WebView reports no safe-area insets to CSS: pad the
   *  page clear of the status bar and gesture bar here, behind it the panel colour. */
  private fun keepClearOfSystemBars() {
    val root = findViewById<View>(android.R.id.content)
    root.setBackgroundColor(Color.parseColor("#121d23"))
    ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
      val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }

  /** The shared Music folder (and folder.jpg covers) needs the media permissions; the media
   *  notification needs the notification one on Android 13+. */
  private fun requestAccess() {
    val wanted = if (Build.VERSION.SDK_INT >= 33) {
      arrayOf(Manifest.permission.READ_MEDIA_AUDIO, Manifest.permission.READ_MEDIA_IMAGES, Manifest.permission.POST_NOTIFICATIONS)
    } else {
      arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
    }
    val missing = wanted.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
    if (missing.isNotEmpty()) requestPermissions(missing.toTypedArray(), REQUEST_STORAGE)
  }

  override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (requestCode != REQUEST_STORAGE) return
    val storage = setOf(Manifest.permission.READ_MEDIA_AUDIO, Manifest.permission.READ_EXTERNAL_STORAGE)
    val granted = permissions.indices.any { permissions[it] in storage && grantResults[it] == PackageManager.PERMISSION_GRANTED }
    if (granted) {
      try { nativeStorageGranted() } catch (_: UnsatisfiedLinkError) {}
    }
  }
}
