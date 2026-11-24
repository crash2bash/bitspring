package dev.local.hifi

import android.Manifest
import android.content.ActivityNotFoundException
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.view.View
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import java.lang.ref.WeakReference

class MainActivity : TauriActivity() {
  companion object {
    /** Rust (android_media.rs): storage access was just granted — scan the Music folder. */
    @JvmStatic external fun nativeStorageGranted()
    /** Rust: the folder picker closed. `uri`: the chosen folder's tree URI, null for none. */
    @JvmStatic external fun nativeFolderPicked(uri: String?)
    private const val REQUEST_STORAGE = 1
    private const val REQUEST_FOLDER = 2
    private var current: WeakReference<MainActivity>? = null

    /**
     * Rust (android_media.rs): show the system's folder picker; the answer comes through
     * [nativeFolderPicked]. False when there is no screen to show it on.
     *
     * Only the choice is used, turned into a path: files are read as files, under the audio
     * permission, so nothing has to be kept of the picker's own grant.
     */
    @JvmStatic
    fun pickFolder(): Boolean {
      val activity = current?.get() ?: return false
      activity.runOnUiThread {
        try {
          @Suppress("DEPRECATION")
          activity.startActivityForResult(Intent(Intent.ACTION_OPEN_DOCUMENT_TREE), REQUEST_FOLDER)
        } catch (_: ActivityNotFoundException) {
          // A device without the system's file picker (some TVs).
          folderPicked(null)
        }
      }
      return true
    }

    private fun folderPicked(uri: String?) {
      try { nativeFolderPicked(uri) } catch (_: UnsatisfiedLinkError) {}
    }
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // Light status/navigation icons over the app's dark panel colour.
    enableEdgeToEdge(SystemBarStyle.dark(Color.TRANSPARENT), SystemBarStyle.dark(Color.TRANSPARENT))
    super.onCreate(savedInstanceState)
    current = WeakReference(this)
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
   *  notification needs the notification one on Android 13+. The Google Play build goes without
   *  the images one (see build.gradle.kts): embedded art and the app's own folder still work. */
  private fun requestAccess() {
    val wanted = if (Build.VERSION.SDK_INT >= 33) {
      listOfNotNull(
        Manifest.permission.READ_MEDIA_AUDIO,
        if (BuildConfig.PLAY_STORE) null else Manifest.permission.READ_MEDIA_IMAGES,
        Manifest.permission.POST_NOTIFICATIONS,
      ).toTypedArray()
    } else {
      arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
    }
    val missing = wanted.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
    if (missing.isNotEmpty()) requestPermissions(missing.toTypedArray(), REQUEST_STORAGE)
  }

  @Deprecated("Deprecated in Java")
  override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
    @Suppress("DEPRECATION")
    super.onActivityResult(requestCode, resultCode, data)
    if (requestCode != REQUEST_FOLDER) return
    folderPicked(if (resultCode == RESULT_OK) data?.data?.toString() else null)
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
