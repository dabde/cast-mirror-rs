package de.tisth.castmirror;

import android.app.Activity;
import android.graphics.Color;
import android.media.MediaCodec;
import android.media.MediaFormat;
import android.os.Bundle;
import android.os.SystemClock;
import android.util.Log;
import android.view.Gravity;
import android.view.SurfaceHolder;
import android.view.Surface;
import android.view.SurfaceView;
import android.view.TextureView;
import android.graphics.SurfaceTexture;
import android.view.WindowManager;
import android.widget.FrameLayout;
import android.widget.TextView;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.SocketAddress;
import java.net.SocketTimeoutException;
import java.nio.ByteBuffer;
import java.util.Collections;
import java.util.List;
import java.util.HashMap;
import java.util.TreeMap;

/**
 * Low-latency receiver for `cast-mirror --direct`: UDP frames straight into the hardware decoder,
 * each decoded frame rendered at once (no jitter buffer).
 *
 * Packet (v2): "CM" | 2 | flags (bit0 key, bit1 HEVC, bit2 retransmit) | frame u32 | index u16 | count u16 |
 *   seq u32 | send ms u32 | payload.
 * Back to the sender: 'K' key frame request; 'N' frame u32, n u16, n x index u16 (missing packets;
 *   n = 0xffff: the whole frame);
 * 'R' every 100 ms: received u32, expected u32, minimum one-way delay ms i32, frames given up u32.
 */
public class MainActivity extends Activity implements SurfaceHolder.Callback, TextureView.SurfaceTextureListener {
    static final int PORT = 7010;
    static final int HEADER = 20;
    static final String TAG = "CastMirror";

    private volatile boolean running;
    private Thread receiver;
    private TextView label;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        // Asks the TV for its game-mode-like path (ALLM); the TV may ignore it.
        getWindow().setPreferMinimalPostProcessing(true);
        FrameLayout root = new FrameLayout(this);
        // Default: composite decoded frames through the GPU (graphics plane, like a game) instead of
        // the TV's video plane, which runs its heavier picture processing.
        if (getIntent().getBooleanExtra("texture", true)) {
            TextureView view = new TextureView(this);
            view.setSurfaceTextureListener(this);
            root.addView(view);
        } else {
            SurfaceView view = new SurfaceView(this);
            view.getHolder().addCallback(this);
            root.addView(view);
        }
        label = new TextView(this);
        label.setTextColor(Color.WHITE);
        label.setBackgroundColor(Color.BLACK); // Covers the last frame once the sender is gone.
        label.setTextSize(28);
        label.setGravity(Gravity.CENTER);
        label.setText("Cast Mirror receiver\n" + localAddress() + ":" + PORT + "\n\nwaiting for cast-mirror --direct");
        root.addView(label, new FrameLayout.LayoutParams(-1, -1));
        setContentView(root);
    }

    @Override
    public void surfaceCreated(SurfaceHolder holder) {
        start(holder.getSurface());
    }

    @Override
    public void onSurfaceTextureAvailable(SurfaceTexture texture, int width, int height) {
        start(new Surface(texture));
    }

    private void start(Surface surface) {
        running = true;
        receiver = new Thread(() -> receive(surface), "receiver");
        receiver.start();
    }

    @Override
    public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {}

    @Override
    public void onSurfaceTextureSizeChanged(SurfaceTexture texture, int width, int height) {}

    @Override
    public void onSurfaceTextureUpdated(SurfaceTexture texture) {}

    @Override
    public boolean onSurfaceTextureDestroyed(SurfaceTexture texture) {
        stop();
        return true;
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        stop();
    }

    private void stop() {
        running = false;
        if (receiver != null) {
            try {
                receiver.join(1000);
            } catch (InterruptedException ignored) {
            }
        }
    }

    private MediaCodec codec;
    private boolean codecHevc;

    // Feedback is best effort: a failed send (e.g. Wi-Fi briefly gone) must not end the receive loop.
    private static void send(DatagramSocket socket, byte[] data, int length, SocketAddress to) {
        try {
            socket.send(new DatagramPacket(data, length, to));
        } catch (IOException e) {
            Log.w(TAG, "feedback not sent: " + e);
        }
    }

    private void receive(Surface surface) {
        try (DatagramSocket socket = new DatagramSocket(PORT)) {
            socket.setSoTimeout(2);
            socket.setReceiveBufferSize(4 << 20);
            byte[] buf = new byte[2048];
            DatagramPacket packet = new DatagramPacket(buf, buf.length);
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            TreeMap<Long, Frame> pending = new TreeMap<>();
            long next = -1;   // Next frame id to decode.
            long newest = -1; // Highest frame id seen.
            boolean needKey = true;
            long lastAsk = 0;
            long lastReport = 0;
            long lastPacket = 0;
            SocketAddress sender = null;
            // Counters for the sender's rate control, reset with every report.
            long seqHigh = -1;
            long seqAtReport = -1;
            int received = 0;
            long owdMin = Long.MAX_VALUE;
            int unrecovered = 0;
            HashMap<Long, long[]> wholeNacks = new HashMap<>(); // id -> {last sent, attempts}

            while (running) {
                if (codec != null) {
                    try {
                        drain(codec, info);
                    } catch (Exception e) {
                        // A decoder error must not end the receiver thread: reset and wait for a key frame.
                        Log.e(TAG, "drain failed, resetting decoder", e);
                        codec.release();
                        codec = null;
                        needKey = true;
                    }
                }
                long now = SystemClock.elapsedRealtime();
                boolean got = true;
                try {
                    socket.receive(packet);
                } catch (SocketTimeoutException e) {
                    got = false;
                }
                int n = packet.getLength();
                // One sender at a time: another address/port only takes over once the current one has been
                // silent for 300 ms (a restart), so two concurrent senders don't ping-pong the decoder.
                boolean foreign = got && sender != null && !sender.equals(packet.getSocketAddress())
                        && now - lastPacket < 300;
                if (got && !foreign && n >= HEADER && buf[0] == 'C' && buf[1] == 'M' && buf[2] == 2) {
                    sender = packet.getSocketAddress();
                    lastPacket = now;
                    int flags = buf[3];
                    long id = u32(buf, 4);
                    int index = u16(buf, 8);
                    int count = u16(buf, 10);
                    long seq = u32(buf, 12);
                    long sent = u32(buf, 16);
                    // A new sender session: ids start at a random value, so any big jump means a restart.
                    if (next >= 0 && Math.abs(id - next) > 1000) {
                        Log.i(TAG, "new sender session");
                        pending.clear();
                        next = -1;
                        newest = -1;
                        needKey = true;
                        seqHigh = -1;
                    }
                    if ((flags & 4) == 0) { // Retransmissions don't count for loss or delay.
                        if (seqHigh < 0) {
                            seqAtReport = seq - 1;
                        }
                        seqHigh = Math.max(seqHigh, seq);
                        received++;
                        // Minimum over the report: the first packet of a frame, free of the frame's own burst.
                        // The clock offset is constant; the sender only uses the trend.
                        owdMin = Math.min(owdMin, now - sent);
                    }
                    if (next < 0) {
                        next = id;
                    }
                    if (count > 0 && index < count && id >= next) {
                        Frame f = pending.get(id);
                        if (f == null) {
                            f = new Frame(id, count, flags & 3, now);
                            pending.put(id, f);
                        }
                        if (count == f.parts.length) { // A corrupt or foreign packet may disagree on count.
                            f.put(index, buf, HEADER, n - HEADER);
                        }
                        newest = Math.max(newest, id);
                    }
                }
                if (sender == null) {
                    continue;
                }
                // Sender gone: drop everything, show the waiting screen instead of a stale picture.
                if (now - lastPacket > 1500) {
                    Log.i(TAG, "sender gone, waiting");
                    pending.clear();
                    next = -1;
                    newest = -1;
                    needKey = true;
                    seqHigh = -1;
                    sender = null;
                    if (codec != null) {
                        codec.release();
                        codec = null;
                    }
                    runOnUiThread(() -> label.setVisibility(TextView.VISIBLE));
                    continue;
                }

                // NACK holes: below the highest index seen, or a frame's tail once a newer frame arrived.
                for (Frame f : pending.values()) {
                    if (f.complete() || f.nacks >= 3 || now - f.lastNack < 25) {
                        continue;
                    }
                    int upTo = f.id < newest ? f.parts.length : f.maxIndex;
                    ByteBuffer nack = ByteBuffer.allocate(7 + 2 * upTo);
                    nack.put((byte) 'N').putInt((int) f.id).putShort((short) 0);
                    int missing = 0;
                    for (int i = 0; i < upTo && missing < 500; i++) {
                        if (f.parts[i] == null) {
                            nack.putShort((short) i);
                            missing++;
                        }
                    }
                    if (missing > 0) {
                        nack.putShort(5, (short) missing);
                        send(socket, nack.array(), nack.position(), sender);
                        f.nacks++;
                        f.lastNack = now;
                    }
                }

                // Whole frames missing before the oldest pending one (e.g. a one-packet frame lost entirely):
                // ask for all of it; 0xffff as the count means "every packet".
                if (!pending.isEmpty()) {
                    Frame head = pending.firstEntry().getValue();
                    final long decoded = next;
                    wholeNacks.keySet().removeIf(m -> m < decoded);
                    for (long m = next; m < head.id && m < next + 8; m++) {
                        long[] st = wholeNacks.computeIfAbsent(m, k -> new long[2]);
                        if (st[1] < 3 && now - st[0] >= 25) {
                            ByteBuffer nack = ByteBuffer.allocate(7);
                            nack.put((byte) 'N').putInt((int) m).putShort((short) 0xffff);
                            send(socket, nack.array(), 7, sender);
                            st[0] = now;
                            st[1]++;
                        }
                    }
                }

                // Decode strictly in order. Give up a frame only if it made no progress for 60 ms while a
                // later frame is complete: on a slow link a big key frame is late, not lost.
                while (!pending.isEmpty()) {
                    Frame head = pending.firstEntry().getValue();
                    if (head.id == next && head.complete()) {
                        pending.pollFirstEntry();
                        next++;
                        if (needKey && (head.flags & 1) == 0) {
                            continue; // The reference chain is broken until the next key frame.
                        }
                        needKey = !decode(surface, head);
                        continue;
                    }
                    boolean whole = head.id != next;
                    boolean laterReady = false;
                    for (Frame f : pending.values()) {
                        laterReady |= f.complete() && (whole ? f.id >= head.id : f.id > head.id);
                    }
                    if (!laterReady || now - (whole ? head.arrived : head.lastProgress) <= 60) {
                        break;
                    }
                    Log.i(TAG, "giving up frame " + next + (whole ? " (never arrived)" : " (incomplete)"));
                    needKey = true;
                    if (head.id != next) {
                        unrecovered += (int) (head.id - next); // Whole frames never arrived.
                        next = head.id;
                    } else {
                        pending.pollFirstEntry();
                        next++;
                        unrecovered++;
                    }
                }

                if (needKey && now - lastAsk > 200) {
                    Log.i(TAG, "requesting key frame");
                    send(socket, new byte[] {'K'}, 1, sender);
                    lastAsk = now;
                }
                if (now - lastReport >= 100 && seqHigh >= 0) {
                    ByteBuffer report = ByteBuffer.allocate(17);
                    report.put((byte) 'R').putInt(received).putInt((int) (seqHigh - seqAtReport));
                    report.putInt(received > 0 ? (int) owdMin : 0).putInt(unrecovered);
                    send(socket, report.array(), 17, sender);
                    lastReport = now;
                    seqAtReport = seqHigh;
                    received = 0;
                    owdMin = Long.MAX_VALUE;
                    unrecovered = 0;
                }
            }
        } catch (Exception e) {
            Log.e(TAG, "receiver failed", e);
        } finally {
            if (codec != null) {
                codec.release();
                codec = null;
            }
        }
    }

    /** Feeds one complete frame to the decoder; false means the reference chain must restart. */
    private boolean decode(Surface surface, Frame frame) {
        boolean hevc = (frame.flags & 2) != 0;
        try {
            if (codec == null || hevc != codecHevc) {
                if (codec != null) {
                    codec.release();
                }
                codec = null;
                codec = open(surface, hevc);
                codecHevc = hevc;
            }
            int in = codec.dequeueInputBuffer(20_000);
            if (in < 0) {
                return false; // Decoder is backed up.
            }
            ByteBuffer input = codec.getInputBuffer(in);
            input.clear();
            frame.copyTo(input);
            codec.queueInputBuffer(in, 0, input.position(), System.nanoTime() / 1000, 0);
            if (label.getVisibility() == TextView.VISIBLE) {
                runOnUiThread(() -> label.setVisibility(TextView.GONE));
            }
            return true;
        } catch (Exception e) {
            // Keep receiving: drop the decoder and start over from the next key frame.
            Log.e(TAG, "decode failed", e);
            if (codec != null) {
                codec.release();
                codec = null;
            }
            return false;
        }
    }

    private static long u32(byte[] b, int o) {
        return ((b[o] & 0xffL) << 24) | ((b[o + 1] & 0xff) << 16) | ((b[o + 2] & 0xff) << 8) | (b[o + 3] & 0xff);
    }

    private static int u16(byte[] b, int o) {
        return ((b[o] & 0xff) << 8) | (b[o + 1] & 0xff);
    }

    private static MediaCodec open(Surface surface, boolean hevc) throws Exception {
        String mime = hevc ? MediaFormat.MIMETYPE_VIDEO_HEVC : MediaFormat.MIMETYPE_VIDEO_AVC;
        // Low-latency mode first; decoders that don't support it (MediaTek here) reject the key.
        // Android's flag, then MediaTek's own key, then plain.
        for (String lowLatency : new String[] {MediaFormat.KEY_LOW_LATENCY, "vdec-lowlatency", "none"}) {
            MediaFormat format = MediaFormat.createVideoFormat(mime, 1920, 1080);
            // Room for 4K up front: switching from 1080p mid-stream errors out on the MediaTek decoder.
            format.setInteger(MediaFormat.KEY_MAX_WIDTH, 3840);
            format.setInteger(MediaFormat.KEY_MAX_HEIGHT, 2160);
            if (!lowLatency.equals("none")) {
                format.setInteger(lowLatency, 1);
                format.setInteger(MediaFormat.KEY_PRIORITY, 0);
            }
            MediaCodec codec = MediaCodec.createDecoderByType(mime);
            try {
                codec.configure(format, surface, null, 0);
                enableVendorLowLatency(codec);
                codec.start();
                Log.i(TAG, "decoder " + codec.getName() + " for " + mime + ", low latency " + lowLatency);
                return codec;
            } catch (Exception e) {
                Log.w(TAG, "configure failed (low latency " + lowLatency + "): " + e);
                codec.release();
            }
        }
        throw new IllegalStateException("no usable decoder for " + mime);
    }

    /** MediaTek rejects KEY_LOW_LATENCY but may offer its own switch; log what exists, turn on what fits. */
    private static void enableVendorLowLatency(MediaCodec codec) {
        try {
            List<String> names = codec.getSupportedVendorParameters();
            Log.i(TAG, "vendor parameters: " + names);
            Bundle on = new Bundle();
            for (String name : names) {
                String n = name.toLowerCase();
                if (n.contains("low") && n.contains("latency") || n.contains("lowlatency")) {
                    on.putInt(name, 1);
                }
            }
            if (!on.isEmpty()) {
                codec.setParameters(on);
                Log.i(TAG, "enabled " + on.keySet());
            }
        } catch (Exception e) {
            Log.w(TAG, "vendor parameters: " + e);
        }
    }

    /** Renders every decoded frame immediately. */
    private static void drain(MediaCodec codec, MediaCodec.BufferInfo info) {
        int out;
        while ((out = codec.dequeueOutputBuffer(info, 0)) >= 0) {
            codec.releaseOutputBuffer(out, true);
        }
    }

    private static String localAddress() {
        try {
            for (NetworkInterface i : Collections.list(NetworkInterface.getNetworkInterfaces())) {
                for (InetAddress a : Collections.list(i.getInetAddresses())) {
                    if (a instanceof Inet4Address && !a.isLoopbackAddress()) {
                        return a.getHostAddress();
                    }
                }
            }
        } catch (Exception ignored) {
        }
        return "?";
    }

    static final class Frame {
        final long id;
        final int flags;
        final long arrived;
        final byte[][] parts;
        long lastProgress;
        int received;
        int maxIndex;
        int nacks;
        long lastNack;

        Frame(long id, int count, int flags, long arrived) {
            this.id = id;
            this.flags = flags;
            this.arrived = arrived;
            this.lastProgress = arrived;
            this.parts = new byte[count][];
        }

        void put(int index, byte[] src, int offset, int length) {
            if (parts[index] == null) {
                byte[] part = new byte[length];
                System.arraycopy(src, offset, part, 0, length);
                parts[index] = part;
                received++;
                maxIndex = Math.max(maxIndex, index);
                lastProgress = SystemClock.elapsedRealtime();
            }
        }

        boolean complete() {
            return received == parts.length;
        }

        void copyTo(ByteBuffer out) {
            for (byte[] part : parts) {
                out.put(part);
            }
        }
    }
}
