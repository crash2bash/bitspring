package dev.local.hifi

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.BitmapFactory
import android.media.MediaMetadata
import android.media.session.MediaSession
import android.media.session.PlaybackState
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock

/**
 * Keeps BitSpring alive with the screen off (a foreground "media playback" service) and shows the
 * media notification / lock-screen player. All audio is in Rust; this only mirrors what Rust
 * tells it (update / progress / clear) and sends button presses back (nativeCommand).
 */
class PlaybackService : Service() {
  companion object {
    private const val CHANNEL = "playback"
    private const val NOTIFICATION_ID = 1
    private const val ACTION_TOGGLE = "dev.local.hifi.TOGGLE"
    private const val ACTION_NEXT = "dev.local.hifi.NEXT"
    private const val ACTION_PREV = "dev.local.hifi.PREV"

    // Commands for Rust (android_media.rs).
    private const val CMD_TOGGLE = 0
    private const val CMD_PLAY = 1
    private const val CMD_PAUSE = 2
    private const val CMD_NEXT = 3
    private const val CMD_PREV = 4
    private const val CMD_SEEK = 5

    @JvmStatic external fun nativeCommand(cmd: Int, arg: Double)

    private var session: MediaSession? = null
    private var metadata: MediaMetadata? = null
    private var playing = false
    private var running = false
    private val main = Handler(Looper.getMainLooper())

    /** Rust calls in from its own threads; MediaSession and notifications live on the main one. */
    private fun onMain(block: () -> Unit) {
      if (Looper.myLooper() == Looper.getMainLooper()) block() else main.post(block)
    }

    private fun session(ctx: Context): MediaSession {
      session?.let { return it }
      val s = MediaSession(ctx.applicationContext, "BitSpring")
      s.setCallback(object : MediaSession.Callback() {
        override fun onPlay() = nativeCommand(CMD_PLAY, 0.0)
        override fun onPause() = nativeCommand(CMD_PAUSE, 0.0)
        override fun onSkipToNext() = nativeCommand(CMD_NEXT, 0.0)
        override fun onSkipToPrevious() = nativeCommand(CMD_PREV, 0.0)
        override fun onSeekTo(pos: Long) = nativeCommand(CMD_SEEK, pos / 1000.0)
      })
      s.isActive = true
      session = s
      return s
    }

    /** A new track. Starts the foreground service if it isn't running. */
    @JvmStatic fun update(ctx: Context, title: String, artist: String?, album: String?, durationMs: Long, cover: ByteArray?) = onMain {
      val b = MediaMetadata.Builder()
        .putString(MediaMetadata.METADATA_KEY_TITLE, title)
        .putLong(MediaMetadata.METADATA_KEY_DURATION, durationMs)
      artist?.let { b.putString(MediaMetadata.METADATA_KEY_ARTIST, it) }
      album?.let { b.putString(MediaMetadata.METADATA_KEY_ALBUM, it) }
      cover?.let { bytes ->
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size)?.let { b.putBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART, it) }
      }
      metadata = b.build()
      session(ctx).setMetadata(metadata)
      if (!running) {
        val i = Intent(ctx, PlaybackService::class.java)
        if (Build.VERSION.SDK_INT >= 26) ctx.startForegroundService(i) else ctx.startService(i)
      } else {
        notifyNow(ctx)
      }
    }

    /** Position and play state; Android runs the clock on from here by itself. */
    @JvmStatic fun progress(ctx: Context, positionMs: Long, isPlaying: Boolean) = onMain {
      playing = isPlaying
      val actions = PlaybackState.ACTION_PLAY or PlaybackState.ACTION_PAUSE or PlaybackState.ACTION_PLAY_PAUSE or
        PlaybackState.ACTION_SKIP_TO_NEXT or PlaybackState.ACTION_SKIP_TO_PREVIOUS or PlaybackState.ACTION_SEEK_TO
      val state = PlaybackState.Builder()
        .setActions(actions)
        .setState(if (isPlaying) PlaybackState.STATE_PLAYING else PlaybackState.STATE_PAUSED, positionMs, if (isPlaying) 1f else 0f, SystemClock.elapsedRealtime())
        .build()
      session(ctx).setPlaybackState(state)
      if (running) notifyNow(ctx)
    }

    /** Nothing loaded: remove the notification and stop being a foreground service. */
    @JvmStatic fun clear(ctx: Context) = onMain {
      ctx.stopService(Intent(ctx, PlaybackService::class.java))
      session?.isActive = false
      session?.release()
      session = null
      metadata = null
    }

    private fun notifyNow(ctx: Context) {
      ctx.getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, build(ctx))
    }

    private fun action(ctx: Context, icon: Int, title: String, action: String): Notification.Action {
      val pi = PendingIntent.getService(ctx, action.hashCode(), Intent(ctx, PlaybackService::class.java).setAction(action),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
      return Notification.Action.Builder(android.graphics.drawable.Icon.createWithResource(ctx, icon), title, pi).build()
    }

    private fun build(ctx: Context): Notification {
      val nm = ctx.getSystemService(NotificationManager::class.java)
      // Created every time: for an existing channel this only renames it, so its name in
      // Settings follows the app's language.
      if (Build.VERSION.SDK_INT >= 26) {
        nm.createNotificationChannel(NotificationChannel(CHANNEL, ctx.getString(R.string.playback_channel), NotificationManager.IMPORTANCE_LOW))
      }
      val open = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.let {
        PendingIntent.getActivity(ctx, 0, it, PendingIntent.FLAG_IMMUTABLE)
      }
      val m = metadata
      val b = if (Build.VERSION.SDK_INT >= 26) Notification.Builder(ctx, CHANNEL) else @Suppress("DEPRECATION") Notification.Builder(ctx)
      return b.setSmallIcon(android.R.drawable.ic_media_play)
        .setContentTitle(m?.getString(MediaMetadata.METADATA_KEY_TITLE) ?: "BitSpring")
        .setContentText(m?.getString(MediaMetadata.METADATA_KEY_ARTIST))
        .setLargeIcon(m?.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART))
        .setContentIntent(open)
        .setOngoing(playing)
        .setVisibility(Notification.VISIBILITY_PUBLIC)
        .addAction(action(ctx, android.R.drawable.ic_media_previous, ctx.getString(R.string.action_prev), ACTION_PREV))
        .addAction(if (playing) action(ctx, android.R.drawable.ic_media_pause, ctx.getString(R.string.action_pause), ACTION_TOGGLE)
                   else action(ctx, android.R.drawable.ic_media_play, ctx.getString(R.string.action_play), ACTION_TOGGLE))
        .addAction(action(ctx, android.R.drawable.ic_media_next, ctx.getString(R.string.action_next), ACTION_NEXT))
        .setStyle(Notification.MediaStyle().setMediaSession(session(ctx).sessionToken).setShowActionsInCompactView(0, 1, 2))
        .build()
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    when (intent?.action) {
      ACTION_TOGGLE -> nativeCommand(CMD_TOGGLE, 0.0)
      ACTION_NEXT -> nativeCommand(CMD_NEXT, 0.0)
      ACTION_PREV -> nativeCommand(CMD_PREV, 0.0)
    }
    if (!running) {
      running = true
      val n = build(this)
      if (Build.VERSION.SDK_INT >= 29) startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK)
      else startForeground(NOTIFICATION_ID, n)
    }
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    running = false
    super.onDestroy()
  }
}
