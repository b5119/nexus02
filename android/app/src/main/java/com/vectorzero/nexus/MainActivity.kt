package com.vectorzero.nexus

import android.content.Intent
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.TextView
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.app.AppCompatActivity
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.RecyclerView
import com.vectorzero.nexus.databinding.ActivityMainBinding

/** Lists paired hosts; tap to stream, long-press to unpair. */
class MainActivity : AppCompatActivity() {

    private lateinit var binding: ActivityMainBinding
    private val adapter = HostAdapter()

    /** Paired computers currently answering on the network, by device id. */
    private val online = HashMap<String, NearbyHost>()
    private lateinit var discovery: NearbyHosts

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = ActivityMainBinding.inflate(layoutInflater)
        setContentView(binding.root)
        setSupportActionBar(binding.toolbar)

        binding.hostList.layoutManager = LinearLayoutManager(this)
        binding.hostList.adapter = adapter

        binding.addHost.setOnClickListener {
            startActivity(Intent(this, PairingActivity::class.java))
        }

        discovery = NearbyHosts(this) { nearby ->
            online.clear()
            for (host in nearby) online[host.deviceId] = host
            // A computer that moved to a new address (DHCP) keeps working: remember the new one.
            for (host in nearby) HostStore.updateAddress(this, host.deviceId, host.address)
            refresh()
        }
    }

    override fun onResume() {
        super.onResume()
        refresh()
        discovery.start()
    }

    override fun onPause() {
        discovery.stop()
        online.clear()
        super.onPause()
    }

    /** Reloads the list, with computers that are online first. */
    private fun refresh() {
        val hosts = HostStore.loadHosts(this).sortedByDescending { it.hostDeviceId in online }
        adapter.submit(hosts)
        binding.emptyState.visibility = if (hosts.isEmpty()) View.VISIBLE else View.GONE
        binding.hostList.visibility = if (hosts.isEmpty()) View.GONE else View.VISIBLE
    }

    private fun unpair(host: PairedHost) {
        AlertDialog.Builder(this)
            .setMessage(getString(R.string.unpair_confirmation))
            .setPositiveButton("Unpair") { _, _ ->
                HostStore.removeHost(this, host.id)
                refresh()
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private inner class HostAdapter : RecyclerView.Adapter<HostAdapter.Holder>() {

        private val items = mutableListOf<PairedHost>()

        fun submit(hosts: List<PairedHost>) {
            items.clear()
            items.addAll(hosts)
            notifyDataSetChanged()
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): Holder {
            val v = LayoutInflater.from(parent.context)
                .inflate(R.layout.item_paired_host, parent, false)
            return Holder(v)
        }

        override fun onBindViewHolder(holder: Holder, position: Int) {
            val host = items[position]
            holder.name.text = host.name
            holder.address.text = host.address
            holder.deviceId.text = host.hostDeviceId.take(8).uppercase().chunked(4).joinToString(" ")
            holder.avatar.text = host.name.firstOrNull()?.uppercase() ?: "?"
            val isOnline = host.hostDeviceId in online
            holder.status.text = getString(if (isOnline) R.string.status_online else R.string.status_offline)
            holder.status.setTextColor(
                getColor(if (isOnline) R.color.nexus_online else R.color.nexus_offline)
            )
            holder.status.setCompoundDrawablesRelativeWithIntrinsicBounds(
                if (isOnline) R.drawable.bg_status_dot else 0, 0, 0, 0
            )
            holder.status.compoundDrawablePadding = 12
            holder.itemView.alpha = if (isOnline) 1f else 0.7f
            holder.itemView.setOnClickListener {
                startActivity(
                    Intent(this@MainActivity, StreamActivity::class.java)
                        .putExtra(StreamActivity.EXTRA_HOST_ID, host.id)
                )
            }
            holder.itemView.setOnLongClickListener {
                unpair(host)
                true
            }
        }

        override fun getItemCount(): Int = items.size

        inner class Holder(itemView: View) : RecyclerView.ViewHolder(itemView) {
            val name: TextView = itemView.findViewById(R.id.hostName)
            val address: TextView = itemView.findViewById(R.id.hostAddress)
            val deviceId: TextView = itemView.findViewById(R.id.hostDeviceId)
            val avatar: TextView = itemView.findViewById(R.id.hostAvatar)
            val status: TextView = itemView.findViewById(R.id.hostStatus)
        }
    }
}
