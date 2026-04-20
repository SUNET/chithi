package `in`.kushaldas.chithi

import android.net.Uri
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.browser.customtabs.CustomTabsIntent

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  /**
   * Invoked from Rust via JNI (see `commands::oauth::open_oauth_url`).
   * A Custom Tab keeps this activity in the task stack, so in-flight
   * HTTP polls against the OIDC token endpoint don't stall the way they
   * do when the browser is launched as a separate task.
   */
  @Suppress("unused")
  fun openCustomTab(url: String) {
    runOnUiThread {
      val intent = CustomTabsIntent.Builder()
        .setShowTitle(true)
        .build()
      intent.launchUrl(this, Uri.parse(url))
    }
  }
}
