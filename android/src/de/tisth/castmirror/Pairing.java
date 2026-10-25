package de.tisth.castmirror;

import android.content.SharedPreferences;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.security.interfaces.ECPublicKey;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.ECPoint;
import java.security.spec.ECPublicKeySpec;
import java.util.Arrays;

import javax.crypto.KeyAgreement;
import javax.crypto.Mac;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.PBEKeySpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * Pairing with a Mac and agreeing on session keys over the network, so casting works without ADB.
 * Wire format and key derivation: see src/pairing.rs on the sender. Requests arrive as "CA" + type; the
 * replies start with the type byte.
 */
final class Pairing {
    interface Listener {
        /** A Mac asks to pair: show the code. Null when pairing ended. */
        void showCode(String mac, String code);

        /** A paired Mac started a session: from now on use this AES key. */
        void sessionKey(byte[] key);
    }

    private static final String CODE_CHARS = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"; // Crockford base32
    private static final int ROUNDS = 100_000;

    private final SharedPreferences prefs;
    private final byte[] deviceId;
    private final String deviceName;
    private final Listener listener;
    private final SecureRandom random = new SecureRandom();

    // The request currently answered, so a resent copy gets the same reply (and the same code).
    private byte[] lastRequest;
    private byte[] lastReply;
    private byte[] pendingLongTerm;
    private String pendingMac;
    private String pendingMacName;
    private byte[] sessionLongTerm;
    private byte[] sessionNonceM;
    private byte[] sessionNonceA;

    Pairing(SharedPreferences prefs, String deviceName, Listener listener) {
        this.prefs = prefs;
        this.deviceName = deviceName;
        this.listener = listener;
        String id = prefs.getString("device_id", null);
        if (id == null) {
            id = hex(bytes(16));
            prefs.edit().putString("device_id", id).apply();
        }
        this.deviceId = unhex(id);
    }

    String deviceIdHex() {
        return hex(deviceId);
    }

    /** Handles a "CA" request (`buf[0..n]`); returns the reply, or null for anything invalid. */
    byte[] handle(byte[] buf, int n) {
        byte[] request = Arrays.copyOf(buf, n);
        if (lastRequest != null && Arrays.equals(request, lastRequest)) {
            return lastReply;
        }
        try {
            byte[] reply = null;
            switch (buf[2]) {
                case 'p': reply = pair(request); break;
                case 'q': reply = confirm(request); break;
                case 's': reply = session(request); break;
                case 't': reply = sessionProof(request); break;
                default: break;
            }
            if (reply != null) {
                lastRequest = request;
                lastReply = reply;
            }
            return reply;
        } catch (Exception e) {
            android.util.Log.w(MainActivity.TAG, "pairing: " + e);
            return null;
        }
    }

    // 'p' mac id(16) | mac public(65) | mac nonce(16) | mac name
    private byte[] pair(byte[] r) throws Exception {
        if (r.length < 3 + 16 + 65 + 16) {
            return null;
        }
        byte[] macId = Arrays.copyOfRange(r, 3, 19);
        byte[] macPublic = Arrays.copyOfRange(r, 19, 84);
        byte[] macNonce = Arrays.copyOfRange(r, 84, 100);
        String macName = new String(r, 100, r.length - 100, StandardCharsets.UTF_8);

        KeyPairGenerator generator = KeyPairGenerator.getInstance("EC");
        generator.initialize(new ECGenParameterSpec("secp256r1"));
        KeyPair pair = generator.generateKeyPair();
        ECPublicKey own = (ECPublicKey) pair.getPublic();
        byte[] appPublic = encode(own);
        ECPublicKey peer = (ECPublicKey) KeyFactory.getInstance("EC").generatePublic(
                new ECPublicKeySpec(decode(macPublic), own.getParams()));
        KeyAgreement agreement = KeyAgreement.getInstance("ECDH");
        agreement.init(pair.getPrivate());
        agreement.doPhase(peer, true);
        byte[] secret = agreement.generateSecret();

        StringBuilder code = new StringBuilder();
        for (int i = 0; i < 10; i++) {
            code.append(CODE_CHARS.charAt(random.nextInt(32)));
        }
        byte[] appNonce = bytes(16);
        byte[] codeKey = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256")
                .generateSecret(new PBEKeySpec(code.toString().toCharArray(), concat(macNonce, appNonce), ROUNDS, 256))
                .getEncoded();
        pendingLongTerm = hmac(codeKey, "cast-mirror pairing".getBytes(StandardCharsets.US_ASCII), secret, macPublic,
                appPublic);
        pendingMac = hex(macId);
        pendingMacName = macName;
        listener.showCode(macName, code.substring(0, 5) + "-" + code.substring(5));
        return concat(new byte[] {'p'}, deviceId, appPublic, appNonce, hmac(pendingLongTerm, ascii("app")),
                deviceName.getBytes(StandardCharsets.UTF_8));
    }

    // 'q' HMAC(lt, "mac"): the Mac had the right code.
    private byte[] confirm(byte[] r) throws Exception {
        if (pendingLongTerm == null || r.length < 3 + 32) {
            return null;
        }
        if (!MessageDigest.isEqual(Arrays.copyOfRange(r, 3, 35), hmac(pendingLongTerm, ascii("mac")))) {
            return null;
        }
        prefs.edit().putString("mac." + pendingMac, hex(pendingLongTerm))
                .putString("name." + pendingMac, pendingMacName).apply();
        pendingLongTerm = null;
        listener.showCode(null, null);
        return new byte[] {'q', 1};
    }

    // 's' mac id(16) | nonce m(16)
    private byte[] session(byte[] r) throws Exception {
        if (r.length < 3 + 32) {
            return null;
        }
        String stored = prefs.getString("mac." + hex(Arrays.copyOfRange(r, 3, 19)), null);
        if (stored == null) {
            return concat(new byte[] {'u'}, deviceId);
        }
        sessionLongTerm = unhex(stored);
        sessionNonceM = Arrays.copyOfRange(r, 19, 35);
        sessionNonceA = bytes(16);
        return concat(new byte[] {'s'}, deviceId, sessionNonceA,
                hmac(sessionLongTerm, ascii("s2"), sessionNonceM, sessionNonceA));
    }

    // 't' HMAC(lt, "s3" m a): only then switch keys, so a replayed 's' can't break a running session.
    private byte[] sessionProof(byte[] r) throws Exception {
        if (sessionLongTerm == null || r.length < 3 + 32) {
            return null;
        }
        byte[] expected = hmac(sessionLongTerm, ascii("s3"), sessionNonceM, sessionNonceA);
        if (!MessageDigest.isEqual(Arrays.copyOfRange(r, 3, 35), expected)) {
            return null;
        }
        listener.sessionKey(Arrays.copyOf(hmac(sessionLongTerm, ascii("key"), sessionNonceM, sessionNonceA), 16));
        sessionLongTerm = null;
        return new byte[] {'t', 1};
    }

    private static byte[] hmac(byte[] key, byte[]... parts) throws Exception {
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec(key, "HmacSHA256"));
        for (byte[] p : parts) {
            mac.update(p);
        }
        return mac.doFinal();
    }

    // Uncompressed P-256 point: 0x04 | x(32) | y(32), as ring's ECDH_P256 uses.
    private static byte[] encode(ECPublicKey key) {
        return concat(new byte[] {4}, fixed(key.getW().getAffineX()), fixed(key.getW().getAffineY()));
    }

    private static ECPoint decode(byte[] point) {
        if (point.length != 65 || point[0] != 4) {
            throw new IllegalArgumentException("not an uncompressed P-256 point");
        }
        return new ECPoint(new BigInteger(1, Arrays.copyOfRange(point, 1, 33)),
                new BigInteger(1, Arrays.copyOfRange(point, 33, 65)));
    }

    private static byte[] fixed(BigInteger v) {
        byte[] raw = v.toByteArray();
        byte[] out = new byte[32];
        int len = Math.min(raw.length, 32);
        System.arraycopy(raw, raw.length - len, out, 32 - len, len);
        return out;
    }

    private byte[] bytes(int n) {
        byte[] b = new byte[n];
        random.nextBytes(b);
        return b;
    }

    private static byte[] ascii(String s) {
        return s.getBytes(StandardCharsets.US_ASCII);
    }

    static byte[] concat(byte[]... parts) {
        int length = 0;
        for (byte[] p : parts) {
            length += p.length;
        }
        byte[] out = new byte[length];
        int at = 0;
        for (byte[] p : parts) {
            System.arraycopy(p, 0, out, at, p.length);
            at += p.length;
        }
        return out;
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) {
            s.append(String.format("%02x", x));
        }
        return s.toString();
    }

    static byte[] unhex(String s) {
        byte[] b = new byte[s.length() / 2];
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        }
        return b;
    }
}
