package com.vectorzero.nexus

import android.content.Context
import android.util.AttributeSet
import android.view.SurfaceView

/**
 * A [SurfaceView] that letterboxes the video stream to its aspect ratio.
 *
 * In FIT mode the entire frame is visible (bars top/bottom or left/right).
 * In FILL mode the surface crops the stream to exactly fill the screen while
 * preserving the aspect ratio (a centered zoom).
 *
 * Rotation is handled naturally: on a 90° rotate the available space changes
 * and [onMeasure] re-fits to the new dimensions.
 */
class AspectSurfaceView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null
) : SurfaceView(context, attrs) {

    private var videoWidth = 0
    private var videoHeight = 0
    var fillMode = false

    fun setVideoSize(width: Int, height: Int) {
        if (width <= 0 || height <= 0) return
        videoWidth = width
        videoHeight = height
        requestLayout()
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        if (videoWidth <= 0 || videoHeight <= 0) {
            super.onMeasure(widthMeasureSpec, heightMeasureSpec)
            return
        }

        val availW = MeasureSpec.getSize(widthMeasureSpec)
        val availH = MeasureSpec.getSize(heightMeasureSpec)
        if (availW <= 0 || availH <= 0) {
            super.onMeasure(widthMeasureSpec, heightMeasureSpec)
            return
        }

        val videoAspect = videoWidth.toFloat() / videoHeight.toFloat()
        val screenAspect = availW.toFloat() / availH.toFloat()

        val w: Int
        val h: Int
        if (fillMode) {
            // Fill the screen: match the screen aspect by letting the stream
            // crop (scale beyond bounds), then center via FIT_XY on the surface.
            if (screenAspect > videoAspect) {
                w = availW
                h = (availW / videoAspect).toInt()
            } else {
                h = availH
                w = (availH * videoAspect).toInt()
            }
        } else {
            // Fit the whole frame: contain within the screen.
            if (screenAspect > videoAspect) {
                h = availH
                w = (availH * videoAspect).toInt()
            } else {
                w = availW
                h = (availW / videoAspect).toInt()
            }
        }

        val wSpec = MeasureSpec.makeMeasureSpec(w, MeasureSpec.EXACTLY)
        val hSpec = MeasureSpec.makeMeasureSpec(h, MeasureSpec.EXACTLY)
        super.onMeasure(wSpec, hSpec)
        setMeasuredDimension(w, h)
    }
}