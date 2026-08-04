package com.vectorzero.nexus

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

data class PairedHost(
    val id: String,
    val name: String,
    val address: String,
    val hostDeviceId: String,
    val certPem: String,
    val authToken: String
)

/** Persists paired hosts (cert + token included) in SharedPreferences. */
object HostStore {
    private const val PREFS = "nexus_prefs"
    private const val KEY_HOSTS = "paired_hosts"

    fun loadHosts(context: Context): List<PairedHost> {
        val raw = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .getString(KEY_HOSTS, "[]") ?: "[]"
        val arr = JSONArray(raw)
        return buildList {
            for (i in 0 until arr.length()) {
                val o = arr.getJSONObject(i)
                add(
                    PairedHost(
                        id = o.getString("id"),
                        name = o.getString("name"),
                        address = o.getString("address"),
                        hostDeviceId = o.getString("host_device_id"),
                        certPem = o.getString("cert_pem"),
                        authToken = o.getString("auth_token")
                    )
                )
            }
        }
    }

    fun saveHost(context: Context, host: PairedHost) {
        val hosts = loadHosts(context).toMutableList()
        hosts.removeAll { it.id == host.id }
        hosts.add(0, host)
        persist(context, hosts)
    }

    fun removeHost(context: Context, id: String) {
        persist(context, loadHosts(context).filterNot { it.id == id })
    }

    private fun persist(context: Context, hosts: List<PairedHost>) {
        val arr = JSONArray()
        hosts.forEach { h ->
            arr.put(
                JSONObject()
                    .put("id", h.id)
                    .put("name", h.name)
                    .put("address", h.address)
                    .put("host_device_id", h.hostDeviceId)
                    .put("cert_pem", h.certPem)
                    .put("auth_token", h.authToken)
            )
        }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(KEY_HOSTS, arr.toString())
            .apply()
    }
}
