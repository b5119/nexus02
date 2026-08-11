package com.vectorzero.nexus

import android.view.MotionEvent
import nexus.stream.v1.StreamServiceOuterClass.InputAction
import nexus.stream.v1.StreamServiceOuterClass.InputEvent
import nexus.stream.v1.StreamServiceOuterClass.InputEventType

/**
 * Translates Android touch events into InputEvent messages in video-frame
 * coordinates; the host maps those onto its absolute input devices.
 */
class InputHandler(private val onEvent: (InputEvent) -> Unit) {

    private var videoWidth = 0
    private var videoHeight = 0

    fun setVideoSize(width: Int, height: Int) {
        videoWidth = width
        videoHeight = height
    }

    fun handleTouch(event: MotionEvent, viewWidth: Int, viewHeight: Int) {
        if (videoWidth <= 0 || videoHeight <= 0) return
        val action = when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> InputAction.PRESS
            MotionEvent.ACTION_MOVE -> InputAction.MOVE
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> InputAction.RELEASE
            else -> return
        }
        val x = (event.x / viewWidth * videoWidth).toInt().coerceIn(0, videoWidth)
        val y = (event.y / viewHeight * videoHeight).toInt().coerceIn(0, videoHeight)
        onEvent(
            InputEvent.newBuilder()
                .setEventType(InputEventType.TOUCH)
                .setAction(action)
                .setX(x)
                .setY(y)
                .build()
        )
    }
}
