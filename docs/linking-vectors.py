#!/usr/bin/env python3
"""Works out the byte-level test vectors in linking-design.md, independently of the Rust
that will be written to them. Needs Python's `cryptography` package and nothing else.

    python3 docs/linking-vectors.py

It is its own small implementation of Noise_KK_25519_ChaChaPoly_BLAKE2s (revision 34 of the
Noise specification), and before printing anything it checks that implementation against the
published vector for that protocol from the cacophony set (as carried in snow's
tests/vectors/cacophony.txt at v0.10.0): both handshake messages, the handshake hash and
four transport messages. It also checks X25519 against RFC 7748 section 6.1. A wrong answer
here cannot pass unnoticed.

What it does not cover: the decisions (leases, operation ids, revisions, membership). Those
are the scenario vectors in section 12 of the design, which are tables of what arrives and
what must be decided, and are for `core/`'s own tests."""
import base64, hashlib, hmac, struct
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey, X25519PublicKey
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

h = bytes.fromhex
def public(secret): return X25519PrivateKey.from_private_bytes(secret).public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
def dh(secret, their_public): return X25519PrivateKey.from_private_bytes(secret).exchange(X25519PublicKey.from_public_bytes(their_public))

# ---- Noise_KK_25519_ChaChaPoly_BLAKE2s ---------------------------------------------------------
NAME = b'Noise_KK_25519_ChaChaPoly_BLAKE2s'
def HASH(data): return hashlib.blake2s(data).digest()
def HMAC(key, data): return hmac.new(key, data, hashlib.blake2s).digest()
def hkdf2(chaining_key, material):
    temp = HMAC(chaining_key, material)
    one = HMAC(temp, b'\x01')
    return one, HMAC(temp, one + b'\x02')
def nonce(n): return b'\x00' * 4 + struct.pack('<Q', n)

class Symmetric:
    def __init__(self, prologue, initiator_static_public, responder_static_public):
        self.h = NAME.ljust(32, b'\x00') if len(NAME) <= 32 else HASH(NAME)
        self.ck, self.k, self.n = self.h, None, 0
        self.mix_hash(prologue)
        self.mix_hash(initiator_static_public)      # KK's pre-messages: -> s, <- s
        self.mix_hash(responder_static_public)
    def mix_hash(self, data): self.h = HASH(self.h + data)
    def mix_key(self, material): self.ck, self.k = hkdf2(self.ck, material); self.n = 0
    def encrypt_and_hash(self, plaintext):
        sealed = ChaCha20Poly1305(self.k).encrypt(nonce(self.n), plaintext, self.h); self.n += 1
        self.mix_hash(sealed); return sealed
    def decrypt_and_hash(self, sealed):
        plaintext = ChaCha20Poly1305(self.k).decrypt(nonce(self.n), sealed, self.h); self.n += 1
        self.mix_hash(sealed); return plaintext
    def split(self): return hkdf2(self.ck, b'')

def kk(prologue, i_static, i_ephemeral, r_static, r_ephemeral, payload1=b'', payload2=b''):
    """Both sides of the handshake at once. Returns the two messages, the handshake hash, and
    the keys for initiator-to-responder and responder-to-initiator transport."""
    i_pub, r_pub = public(i_static), public(r_static)
    a, b = Symmetric(prologue, i_pub, r_pub), Symmetric(prologue, i_pub, r_pub)
    # -> e, es, ss
    e = public(i_ephemeral); a.mix_hash(e)
    a.mix_key(dh(i_ephemeral, r_pub)); a.mix_key(dh(i_static, r_pub))
    message1 = e + a.encrypt_and_hash(payload1)
    b.mix_hash(message1[:32]); b.mix_key(dh(r_static, message1[:32])); b.mix_key(dh(r_static, i_pub))
    assert b.decrypt_and_hash(message1[32:]) == payload1
    # <- e, ee, se
    e2 = public(r_ephemeral); b.mix_hash(e2)
    b.mix_key(dh(r_ephemeral, e)); b.mix_key(dh(r_ephemeral, i_pub))
    message2 = e2 + b.encrypt_and_hash(payload2)
    a.mix_hash(message2[:32]); a.mix_key(dh(i_ephemeral, message2[:32])); a.mix_key(dh(i_static, message2[:32]))
    assert a.decrypt_and_hash(message2[32:]) == payload2
    assert a.h == b.h and a.split() == b.split()
    to_responder, to_initiator = a.split()
    return message1, message2, a.h, to_responder, to_initiator
def transport(key, n, plaintext): return ChaCha20Poly1305(key).encrypt(nonce(n), plaintext, b'')

# The published vector (cacophony, Noise_KK_25519_ChaChaPoly_BLAKE2s).
m1, m2, hh, k_ir, k_ri = kk(h('4a6f686e2047616c74'),
    h('e61ef9919cde45dd5f82166404bd08e38bceb5dfdfded0a34c8df7ed542214d1'), h('893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a'),
    h('4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893'), h('bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b'),
    h('4c756477696720766f6e204d69736573'), h('4d757272617920526f746862617264'))
assert m1.hex() == 'ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c7944266a5f53784aa3becb0f7485c2759c328937867a4cbaafef07422b0725e098be'
assert m2.hex() == '95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843008aeea5d76d6abcbab87a18502c8a8352d9933ac11e2a7d228038d721e31e'
assert hh.hex() == '1362b8627a00907ce11e558aba8ce7cbca88e83f0e84ce7db5159b1c3e25ab59'
PUBLISHED_TRANSPORT = [  # (payload, ciphertext): initiator, responder, initiator, responder, each side counting from 0
    ('462e20412e20486179656b', '5f92113edf78c3e56e6d67201f5f9e0c8f2930c3e1ffb64ede0358'),
    ('4361726c204d656e676572', '30ebbd9cdcef7f40d99c8cd11e880dac28f5c9e5032c1059b3b56a'),
    ('4a65616e2d426170746973746520536179', 'b011620dc31f88abd1788db50912952fe45da56e9d0907ab2cbce5f609b58b1cf2'),
    ('457567656e2042f6686d20766f6e2042617765726b', 'a0661971e9047b28a815c7b1f62fefb471e4d34bc2a5b48149e7f80c3772b8e4aae8b44baa')]
for at, (payload, sealed) in enumerate(PUBLISHED_TRANSPORT):
    assert transport(k_ir if at % 2 == 0 else k_ri, at // 2, h(payload)).hex() == sealed

# ---- the design's own pieces -------------------------------------------------------------------
def b32(data): return base64.b32encode(data).decode().lower().rstrip('=')
def b64(data): return base64.urlsafe_b64encode(data).decode().rstrip('=')
def device_id(public_key): return hashlib.sha256(b'nexus-music device 3\n' + public_key).digest()[:16]
def account_id(owner, playlist, salt): return hashlib.sha256(b'nexus-music account 3\n' + owner.encode() + b'\n' + playlist.encode() + b'\n' + salt).digest()
def room_name(account): return b32(hashlib.sha256(b'nexus-music room 3\n' + account).digest())
def prologue(account, initiator, responder, boot): return b'nexus-music link 3' + account + device_id(initiator) + device_id(responder) + boot
def frame(kind, channel, counter, body): return b'NML3' + bytes([kind]) + channel + struct.pack('>Q', counter) + body

# RFC 7748, section 6.1: the two devices are its Alice (a PC, opening the channel) and Bob (a phone).
pc_secret, phone_secret = h('77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a'), h('5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb')
pc, phone = public(pc_secret), public(phone_secret)
assert pc.hex() == '8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a' and phone.hex() == 'de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f'
assert dh(pc_secret, phone).hex() == '4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742'

owner, playlist, salt = 'tester', 'AbCdEfGhIjKlMnOpQrStUv', h('000102030405060708090a0b0c0d0e0f')
account = account_id(owner, playlist, salt)
boot = h('f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff')
channel = h('101112131415161718191a1b1c1d1e1f')
pc_ephemeral, phone_ephemeral = h('a0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebf'), h('c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf')
pro = prologue(account, pc, phone, boot)
message1, message2, handshake_hash, to_phone, to_pc = kk(pro, pc_secret, pc_ephemeral, phone_secret, phone_ephemeral)
opening = b'{"t":"open","want":"steer"}'
welcome = b'{"t":"state","rev":7,"lease":"AAECAwQFBgc","playing":true}'
frames = [frame(1, channel, 0, message1), frame(2, channel, 0, message2),
          frame(3, channel, 0, transport(to_phone, 0, opening)), frame(3, channel, 0, transport(to_pc, 0, welcome))]
assert ChaCha20Poly1305(to_phone).decrypt(nonce(0), frames[2][29:], b'') == opening

print('PC      secret', pc_secret.hex()); print('        public', pc.hex(), '=', b64(pc)); print('        id    ', b32(device_id(pc)), '=', device_id(pc).hex())
print('phone   secret', phone_secret.hex()); print('        public', phone.hex(), '=', b64(phone)); print('        id    ', b32(device_id(phone)), '=', device_id(phone).hex())
print('owner', owner, '| playlist', playlist, '| salt', salt.hex(), '=', b64(salt))
print('account       ', account.hex()); print('room          ', room_name(account))
print('phone boot    ', boot.hex(), '=', b64(boot)); print('channel       ', channel.hex())
print('PC ephemeral  ', pc_ephemeral.hex(), '->', public(pc_ephemeral).hex()); print('phone ephemeral', phone_ephemeral.hex(), '->', public(phone_ephemeral).hex())
print('prologue      ', pro.hex())
print('message 1     ', message1.hex()); print('message 2     ', message2.hex()); print('handshake hash', handshake_hash.hex())
print('key PC->phone ', to_phone.hex()); print('key phone->PC ', to_pc.hex())
for name, f in zip(('frame: handshake 1', 'frame: handshake 2', 'frame: PC->phone, 0', 'frame: phone->PC, 0'), frames):
    print(f'{name:22}', f.hex()); print(f'{"  as sent":22}', b64(f))
print('opening plaintext', opening.decode()); print('welcome plaintext', welcome.decode())
