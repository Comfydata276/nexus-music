#!/usr/bin/env python3
"""Works out the test vectors in linking-design.md, independently of the Rust that will be
written to them. Needs Python's `cryptography` package and nothing else.

    python3 docs/linking-vectors.py

It first checks its own XChaCha20-Poly1305 against the published vectors of
draft-irtf-cfrg-xchacha-03 (sections 2.2.1 and A.3.1) and its X25519 against RFC 7748
section 6.1, so a wrong answer here cannot pass unnoticed."""
import base64, hashlib, struct
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

h = bytes.fromhex

# ---- XChaCha20-Poly1305, from ChaCha20-Poly1305 and HChaCha20 ---------------------------------
def rotl(v, c): return ((v << c) & 0xffffffff) | (v >> (32 - c))
def quarter(s, a, b, c, d):
    s[a] = (s[a] + s[b]) & 0xffffffff; s[d] = rotl(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & 0xffffffff; s[b] = rotl(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b]) & 0xffffffff; s[d] = rotl(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & 0xffffffff; s[b] = rotl(s[b] ^ s[c], 7)
def hchacha20(key, nonce16):
    s = list(struct.unpack('<4I', b'expand 32-byte k')) + list(struct.unpack('<8I', key)) + list(struct.unpack('<4I', nonce16))
    for _ in range(10):
        quarter(s, 0, 4, 8, 12); quarter(s, 1, 5, 9, 13); quarter(s, 2, 6, 10, 14); quarter(s, 3, 7, 11, 15)
        quarter(s, 0, 5, 10, 15); quarter(s, 1, 6, 11, 12); quarter(s, 2, 7, 8, 13); quarter(s, 3, 4, 9, 14)
    return struct.pack('<8I', *(s[0:4] + s[12:16]))
def xchacha20poly1305_seal(key, nonce24, plaintext, aad):
    return ChaCha20Poly1305(hchacha20(key, nonce24[:16])).encrypt(b'\x00' * 4 + nonce24[16:], plaintext, aad)
def xchacha20poly1305_open(key, nonce24, sealed, aad):
    return ChaCha20Poly1305(hchacha20(key, nonce24[:16])).decrypt(b'\x00' * 4 + nonce24[16:], sealed, aad)

assert hchacha20(h('000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f'), h('000000090000004a0000000031415927')).hex() == '82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc'
_ct = xchacha20poly1305_seal(h('808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f'), h('404142434445464748494a4b4c4d4e4f5051525354555657'),
                             b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.", h('50515253c0c1c2c3c4c5c6c7'))
assert _ct[:32].hex() == 'bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb' and _ct[-16:].hex() == 'c0875924c1c7987947deafd8780acf49'

# ---- the design's own pieces ------------------------------------------------------------------
def b32(data): return base64.b32encode(data).decode().lower().rstrip('=')
def b64(data): return base64.urlsafe_b64encode(data).decode().rstrip('=')
def public(secret): return X25519PrivateKey.from_private_bytes(secret).public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
def dh(secret, their_public): return X25519PrivateKey.from_private_bytes(secret).exchange(X25519PublicKey.from_public_bytes(their_public))
def device_id(public_key): return hashlib.sha256(b'nexus-music device 2\n' + public_key).digest()[:16]
def room_name(account): return b32(hashlib.sha256(b'nexus-music room 2\n' + account).digest())
KINDS = {'presence': 1, 'ask': 2, 'reply': 3, 'state': 4}
def key(kind, secret, my_public, their_public, sending):
    sender, receiver = (my_public, their_public) if sending else (their_public, my_public)
    return HKDF(hashes.SHA256(), 32, salt=b'nexus-music link 2', info=bytes([KINDS[kind]]) + sender + receiver).derive(dh(secret, their_public))
def header(kind, sender_public, receiver_public, session, n):
    return b'NML2' + bytes([KINDS[kind]]) + device_id(sender_public) + device_id(receiver_public) + session + struct.pack('>Q', n)
def seal(kind, secret, my_public, their_public, session, n, nonce, body):
    head = header(kind, my_public, their_public, session, n)
    return head + nonce + xchacha20poly1305_seal(key(kind, secret, my_public, their_public, True), nonce, body, head)
def open_(kind, secret, my_public, their_public, blob):
    head, nonce, sealed = blob[:61], blob[61:85], blob[85:]
    return xchacha20poly1305_open(key(kind, secret, my_public, their_public, False), nonce, sealed, head)

# RFC 7748, section 6.1: the two devices are its Alice (the PC, asking) and Bob (the phone).
pc_secret, phone_secret = h('77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a'), h('5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb')
pc, phone = public(pc_secret), public(phone_secret)
assert pc.hex() == '8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a' and phone.hex() == 'de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f'
assert dh(pc_secret, phone).hex() == '4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742' == dh(phone_secret, pc).hex()

account = h('000102030405060708090a0b0c0d0e0f')
session = h('f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff')
n = 1_795_000_000_123
nonce = h('404142434445464748494a4b4c4d4e4f5051525354555657')
body = b'{"action":"pause","data":{}}'
blob = seal('ask', pc_secret, pc, phone, session, n, nonce, body)
assert open_('ask', phone_secret, phone, pc, blob) == body

print('PC      secret', pc_secret.hex()); print('        public', pc.hex()); print('        id    ', b32(device_id(pc)), '=', device_id(pc).hex())
print('phone   secret', phone_secret.hex()); print('        public', phone.hex()); print('        id    ', b32(device_id(phone)), '=', device_id(phone).hex())
print('shared (X25519)', dh(pc_secret, phone).hex())
print('account       ', account.hex(), '->', b64(account)); print('room          ', room_name(account))
for kind in KINDS:
    print(f'key {kind:8} PC->phone', key(kind, pc_secret, pc, phone, True).hex())
print('key ask      phone->PC', key('ask', phone_secret, phone, pc, True).hex())
print('ask: session  ', session.hex()); print('     n        ', n); print('     nonce    ', nonce.hex()); print('     body     ', body.decode())
print('     header   ', header('ask', pc, phone, session, n).hex())
print('     sealed   ', blob.hex())
print('     on the wire', b64(blob))
