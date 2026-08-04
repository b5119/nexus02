package com.vectorzero.nexus

import android.content.Intent
import android.os.Bundle
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import androidx.recyclerview.widget.DividerItemDecoration
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

        binding.hostList.layoutManager = LinearLayoutManager(this)
        binding.hostList.adapter = adapter
        binding.hostList.addItemDecoration(DividerItemDecoration(this, LinearLayoutManager.VERTICAL))

        binding.addHost.setOnClickListener {
            startActivity(Intent(this, PairingActivity::class.java))
        }
    }

    override fun onResume() {
        super.onResume()
        adapter.submit(HostStore.loadHosts(this))
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
                .inflate(android.R.layout.simple_list_item_2, parent, false)
            return Holder(v)
        }

        override fun onBindViewHolder(holder: Holder, position: Int) {
            val host = items[position]
            holder.title.text = host.name
            holder.subtitle.text = host.address
            holder.itemView.setOnClickListener {
                startActivity(
                    Intent(this@MainActivity, StreamActivity::class.java)
                        .putExtra(StreamActivity.EXTRA_HOST_ID, host.id)
                )
            }
            holder.itemView.setOnLongClickListener {
                HostStore.removeHost(this@MainActivity, host.id)
                submit(HostStore.loadHosts(this@MainActivity))
                true
            }
        }

        override fun getItemCount(): Int = items.size

        class Holder(itemView: View) : RecyclerView.ViewHolder(itemView) {
            val title: TextView = itemView.findViewById(android.R.id.text1)
            val subtitle: TextView = itemView.findViewById(android.R.id.text2)
        }
    }
}
