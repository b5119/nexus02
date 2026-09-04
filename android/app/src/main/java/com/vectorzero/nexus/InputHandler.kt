package com.vectorzero.nexus

import android.view.KeyEvent
import android.view.MotionEvent
import nexus.stream.v1.StreamServiceOuterClass.InputAction
import nexus.stream.v1.StreamServiceOuterClass.InputEvent
import nexus.stream.v1.StreamServiceOuterClass.InputEventType
import nexus.stream.v1.StreamServiceOuterClass.MouseButton

/**
 * Translates Android input into InputEvent messages for the nexus host.
 *
 * Two interaction modes:
 *  - TOUCH: absolute cursoring — a tap at (x, y) sends an absolute TOUCH
 *    press/release at that point (tap = left click).
 *  - POINTER: the whole screen is a trackpad — drag moves the pointer by
 *    relative deltas, tap = left click, long-press = right click, and a
 *    two-finger drag scrolls.
 *
 * Keyboard events (hardware keyboard on the tablet) are forwarded as Linux
 * evdev keycodes via [KeyCodeMapper].
 */
class InputHandler(private val onEvent: (InputEvent) -> Unit) {

    enum class Mode { TOUCH, POINTER }

    private var mode = Mode.TOUCH
    private var videoWidth = 0
    private var videoHeight = 0

    // Pointer-mode gesture state.
    private var pointerGestureActive = false
    private var longPressFired = false
    private val longPressThresholdMs = 600L
    private var downTime = 0L
    private var lastPointerX = 0f
    private var lastPointerY = 0f

    // Two-finger scroll state.
    private var scrollActive = false
    private var lastScrollX = 0f
    private var lastScrollY = 0f

    fun setVideoSize(width: Int, height: Int) {
        videoWidth = width
        videoHeight = height
    }

    fun setMode(mode: Mode) {
        this.mode = mode
        if (mode == Mode.TOUCH) {
            cancelPointerGesture()
        }
    }

    fun handleTouch(event: MotionEvent, viewWidth: Int, viewHeight: Int) {
        if (videoWidth <= 0 || videoHeight <= 0) return
        if (viewWidth <= 0 || viewHeight <= 0) return

        val pointerCount = event.pointerCount

        if (mode == Mode.TOUCH) {
            handleTouchMode(event, viewWidth, viewHeight)
        } else {
            handlePointerMode(event, viewWidth, viewHeight)
        }
    }

    /** Sends a keyboard event with the Linux evdev keycode. */
    fun sendKey(keyCode: Int, keyDown: Boolean) {
        val linuxCode = KeyCodeMapper.toLinux(keyCode)
        if (linuxCode <= 0) return
        onEvent(
            InputEvent.newBuilder()
                .setEventType(InputEventType.KEYBOARD)
                .setKeyCode(linuxCode)
                .setAction(if (keyDown) InputAction.PRESS else InputAction.RELEASE)
                .build()
        )
    }

    // --- TOUCH mode --------------------------------------------------------

    private fun handleTouchMode(event: MotionEvent, viewWidth: Int, viewHeight: Int) {
        val action = when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> InputAction.PRESS
            MotionEvent.ACTION_MOVE -> InputAction.MOVE
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> InputAction.RELEASE
            else -> return
        }
        if (viewWidth <= 0 || viewHeight <= 0) return
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

    // --- POINTER mode ------------------------------------------------------

    private fun handlePointerMode(event: MotionEvent, viewWidth: Int, viewHeight: Int) {
        when (event.actionMasked) {
            MotionEvent.ACTION_POINTER_DOWN, MotionEvent.ACTION_POINTER_UP,
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_UP -> {
                // Two-finger scroll: when a second finger goes down and at
                // least one remains, switch from pointer-move / tap to scroll.
                scrollActive = event.pointerCount >= 2
                if (scrollActive) {
                    lastScrollX = event.getX(0)
                    lastScrollY = event.getY(0)
                }
            }
            MotionEvent.ACTION_MOVE -> {
                if (scrollActive && event.pointerCount >= 2) {
                    sendScroll(event)
                } else if (scrollActive) {
                    scrollActive = false
                } else {
                    val dx = event.x - lastPointerX
                    val dy = event.y - lastPointerY
                    lastPointerX = event.x
                    lastPointerY = event.y
                    sendPointerMove(dx, dy)
                }
            }
        }

        if (event.actionMasked == MotionEvent.ACTION_DOWN) {
            pointerGestureActive = true
            longPressFired = false
            downTime = event.eventTime
            lastPointerX = event.x
            lastPointerY = event.y
            scheduleLongPress()
        }
        if (event.actionMasked == MotionEvent.ACTION_UP && pointerGestureActive) {
            pointerGestureActive = false
            if (event.pointerCount >= 2) return // was a scroll gesture
            val button =
                if (longPressFired) MouseButton.RIGHT else MouseButton.LEFT
            onEvent(
                InputEvent.newBuilder()
                    .setEventType(InputEventType.MOUSE)
                    .setButton(button)
                    .setAction(InputAction.PRESS)
                    .build()
            )
            onEvent(
                InputEvent.newBuilder()
                    .setEventType(InputEventType.MOUSE)
                    .setButton(button)
                    .setAction(InputAction.RELEASE)
                    .build()
            )
        }
    }

    private fun scheduleLongPress() {
        android.os.Handler().postDelayed({
            if (pointerGestureActive && !longPressFired &&
                System.currentTimeMillis() - downTime >= longPressThresholdMs
            ) {
                longPressFired = true
            }
        }, longPressThresholdMs)
    }

    private fun sendPointerMove(dx: Float, dy: Float) {
        // Relative mouse movement, scaled for tablet-size screens. Raw screen
        // pixels are large; scale down so the host pointer moves modestly.
        val sx = (dx * 0.5f).toInt()
        val sy = (dy * 0.5f).toInt()
        if (sx == 0 && sy == 0) return
        onEvent(
            InputEvent.newBuilder()
                .setEventType(InputEventType.MOUSE)
                // Proto `x`/`y` are uint32; serialized as 32-bit two's complement,
                // and the host casts them back to i32 whch reconstructs negative deltas.
                .setX(sx)
                .setY(sy)
                .setAction(InputAction.MOVE)
                .build()
        )
    }

    private fun sendScroll(event: MotionEvent) {
        val dy = event.getY(0) - lastScrollY
        // Accumulate into wheel units (≈8 px per notch on the host).
        scrollAccum += dy
        val notches = (scrollAccum / 8f).toInt()
        if (notches != 0) {
            onEvent(
                InputEvent.newBuilder()
                    .setEventType(InputEventType.MOUSE)
                    .setScrollDelta(notches)
                    .setAction(InputAction.SCROLL)
                    .build()
            )
            scrollAccum = 0f
        }
        lastScrollX = event.getX(0)
        lastScrollY = event.getY(0)
    }

    private var scrollAccum = 0f

    private fun cancelPointerGesture() {
        pointerGestureActive = false
        longPressFired = false
        scrollActive = false
    }
}

/**
 * Maps Android [KeyEvent] keycodes to Linux evdev keycodes so the host can
 * inject them. Covers the common keyboard layout; unmapped keys are dropped.
 */
object KeyCodeMapper {
    fun toLinux(androidKeyCode: Int): Int = when (androidKeyCode) {
        KeyEvent.KEYCODE_A -> 30
        KeyEvent.KEYCODE_B -> 48
        KeyEvent.KEYCODE_C -> 46
        KeyEvent.KEYCODE_D -> 32
        KeyEvent.KEYCODE_E -> 18
        KeyEvent.KEYCODE_F -> 33
        KeyEvent.KEYCODE_G -> 34
        KeyEvent.KEYCODE_H -> 35
        KeyEvent.KEYCODE_I -> 23
        KeyEvent.KEYCODE_J -> 36
        KeyEvent.KEYCODE_K -> 37
        KeyEvent.KEYCODE_L -> 38
        KeyEvent.KEYCODE_M -> 50
        KeyEvent.KEYCODE_N -> 49
        KeyEvent.KEYCODE_O -> 24
        KeyEvent.KEYCODE_P -> 25
        KeyEvent.KEYCODE_Q -> 16
        KeyEvent.KEYCODE_R -> 19
        KeyEvent.KEYCODE_S -> 31
        KeyEvent.KEYCODE_T -> 20
        KeyEvent.KEYCODE_U -> 22
        KeyEvent.KEYCODE_V -> 47
        KeyEvent.KEYCODE_W -> 17
        KeyEvent.KEYCODE_X -> 45
        KeyEvent.KEYCODE_Y -> 21
        KeyEvent.KEYCODE_Z -> 44
        KeyEvent.KEYCODE_0 -> 11
        KeyEvent.KEYCODE_1 -> 2
        KeyEvent.KEYCODE_2 -> 3
        KeyEvent.KEYCODE_3 -> 4
        KeyEvent.KEYCODE_4 -> 5
        KeyEvent.KEYCODE_5 -> 6
        KeyEvent.KEYCODE_6 -> 7
        KeyEvent.KEYCODE_7 -> 8
        KeyEvent.KEYCODE_8 -> 9
        KeyEvent.KEYCODE_9 -> 10
        KeyEvent.KEYCODE_SPACE -> 57
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> 28
        KeyEvent.KEYCODE_TAB -> 15
        KeyEvent.KEYCODE_DEL -> 14
        KeyEvent.KEYCODE_ESCAPE -> 1
        KeyEvent.KEYCODE_SHIFT_LEFT -> 42
        KeyEvent.KEYCODE_SHIFT_RIGHT -> 54
        KeyEvent.KEYCODE_CTRL_LEFT -> 29
        KeyEvent.KEYCODE_CTRL_RIGHT -> 97
        KeyEvent.KEYCODE_ALT_LEFT -> 56
        KeyEvent.KEYCODE_ALT_RIGHT -> 100
        KeyEvent.KEYCODE_META_LEFT -> 125
        KeyEvent.KEYCODE_META_RIGHT -> 126
        KeyEvent.KEYCODE_CAPS_LOCK -> 58
        KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_PAGE_UP -> 104
        KeyEvent.KEYCODE_DPAD_DOWN, KeyEvent.KEYCODE_PAGE_DOWN -> 109
        KeyEvent.KEYCODE_DPAD_LEFT -> 105
        KeyEvent.KEYCODE_DPAD_RIGHT -> 106
        KeyEvent.KEYCODE_HOME -> 102
        KeyEvent.KEYCODE_MOVE_END -> 107
        KeyEvent.KEYCODE_INSERT -> 110
        KeyEvent.KEYCODE_FORWARD_DEL -> 111
        KeyEvent.KEYCODE_MINUS -> 12
        KeyEvent.KEYCODE_EQUALS -> 13
        KeyEvent.KEYCODE_LEFT_BRACKET -> 26
        KeyEvent.KEYCODE_RIGHT_BRACKET -> 27
        KeyEvent.KEYCODE_BACKSLASH -> 43
        KeyEvent.KEYCODE_SEMICOLON -> 39
        KeyEvent.KEYCODE_APOSTROPHE -> 40
        KeyEvent.KEYCODE_GRAVE -> 41
        KeyEvent.KEYCODE_COMMA -> 51
        KeyEvent.KEYCODE_PERIOD -> 52
        KeyEvent.KEYCODE_SLASH -> 53
        KeyEvent.KEYCODE_F1 -> 59
        KeyEvent.KEYCODE_F2 -> 60
        KeyEvent.KEYCODE_F3 -> 61
        KeyEvent.KEYCODE_F4 -> 62
        KeyEvent.KEYCODE_F5 -> 63
        KeyEvent.KEYCODE_F6 -> 64
        KeyEvent.KEYCODE_F7 -> 65
        KeyEvent.KEYCODE_F8 -> 66
        KeyEvent.KEYCODE_F9 -> 67
        KeyEvent.KEYCODE_F10 -> 68
        KeyEvent.KEYCODE_F11 -> 87
        KeyEvent.KEYCODE_F12 -> 88
        else -> 0
    }
}