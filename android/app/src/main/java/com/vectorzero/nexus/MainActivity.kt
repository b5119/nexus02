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
    }

    override fun onResume() {
        super.onResume()
        val hosts = HostStore.loadHosts(this)
        adapter.submit(hosts)
        binding.emptyState.visibility = if (hosts.isEmpty()) View.VISIBLE else View.GONE
        binding.hostList.visibility = if (hosts.isEmpty()) View.GONE else View.VISIBLE
    }

    private fun unpair(host: PairedHost) {
        AlertDialog.Builder(this)
            .setMessage(getString(R.string.unpair_confirmation))
            .setPositiveButton("Unpair") { _, _ ->
                HostStore.removeHost(this, host.id)
                val hosts = HostStore.loadHosts(this)
                adapter.submit(hosts)
                binding.emptyState.visibility = if (hosts.isEmpty()) View.VISIBLE else View.GONE
                binding.hostList.visibility = if (hosts.isEmpty()) View.GONE else View.VISIBLE
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
        }
    }
}
