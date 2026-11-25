#!/usr/bin/env python3
"""Works out the test vectors in linking-design.md (version 3), independently of the Rust
that will be written to them. Needs Python's `cryptography` package and nothing else.

    python3 docs/linking-vectors.py

Two halves.

The bytes: HKDF-SHA256 and XChaCha20-Poly1305, written out here and checked, before anything
is printed, against RFC 5869 (test cases 1 and 3) and draft-irtf-cfrg-xchacha-03 (the
HChaCha20 vector of 2.2.1 and the AEAD vector of A.3.1). Then the design's own values.

The decisions: a model of the one rule a device applies to a command (section 5.6), small
enough to read, and the reviewers' sequences replayed against it (section 9). `core/`'s
tests must decide each the same way."""
import base64, hashlib, hmac, struct
from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

h = bytes.fromhex
def b32(data): return base64.b32encode(data).decode().lower().rstrip('=')
def b64(data): return base64.urlsafe_b64encode(data).decode().rstrip('=')

# ---- HKDF-SHA256 (RFC 5869) ---------------------------------------------------------------------
def hkdf(ikm, info, length=32, salt=b''):
    prk = hmac.new(salt or b'\x00' * 32, ikm, hashlib.sha256).digest()
    out, block = b'', b''
    while len(out) < length:
        block = hmac.new(prk, block + info + bytes([len(out) // 32 + 1]), hashlib.sha256).digest()
        out += block
    return out[:length]
assert hkdf(h('0b' * 22), h('f0f1f2f3f4f5f6f7f8f9'), 42, h('000102030405060708090a0b0c')).hex() == \
    '3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865'
assert hkdf(h('0b' * 22), b'', 42).hex() == \
    '8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8'

# ---- XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha-03) --------------------------------------------
def hchacha20(key, nonce16):
    s = list(struct.unpack('<4I', b'expand 32-byte k')) + list(struct.unpack('<8I', key)) + list(struct.unpack('<4I', nonce16))
    rot = lambda v, n: ((v << n) | (v >> (32 - n))) & 0xffffffff
    def quarter(a, b, c, d):
        s[a] = (s[a] + s[b]) & 0xffffffff; s[d] = rot(s[d] ^ s[a], 16)
        s[c] = (s[c] + s[d]) & 0xffffffff; s[b] = rot(s[b] ^ s[c], 12)
        s[a] = (s[a] + s[b]) & 0xffffffff; s[d] = rot(s[d] ^ s[a], 8)
        s[c] = (s[c] + s[d]) & 0xffffffff; s[b] = rot(s[b] ^ s[c], 7)
    for _ in range(10):
        quarter(0, 4, 8, 12); quarter(1, 5, 9, 13); quarter(2, 6, 10, 14); quarter(3, 7, 11, 15)
        quarter(0, 5, 10, 15); quarter(1, 6, 11, 12); quarter(2, 7, 8, 13); quarter(3, 4, 9, 14)
    return struct.pack('<8I', *(s[0:4] + s[12:16]))
def _inner(key, nonce24): return ChaCha20Poly1305(hchacha20(key, nonce24[:16])), b'\x00' * 4 + nonce24[16:]
def seal(key, nonce24, plaintext, aad):
    cipher, nonce = _inner(key, nonce24); return cipher.encrypt(nonce, plaintext, aad)
def open_(key, nonce24, sealed, aad):
    cipher, nonce = _inner(key, nonce24); return cipher.decrypt(nonce, sealed, aad)
assert hchacha20(bytes(range(32)), h('000000090000004a0000000031415927')).hex() == \
    '82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc'
assert seal(bytes(range(0x80, 0xa0)), bytes(range(0x40, 0x58)),
            b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.",
            h('50515253c0c1c2c3c4c5c6c7')).hex() == (
    'bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452'
    '2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52e'
    'c0875924c1c7987947deafd8780acf49')

# ---- the design's bytes -------------------------------------------------------------------------
ASK, TELL = 1, 2
def seal_key(secret): return hkdf(secret, b'nexus-music link 4 seal')
def room_name(secret): return b32(hkdf(secret, b'nexus-music link 4 room'))
def aad(kind, sender, recipient): return b'NML4' + bytes([kind]) + sender + recipient
def frame(key, kind, nonce, sender, recipient, body):
    return b'NML4' + bytes([kind]) + nonce + seal(key, nonce, body, aad(kind, sender, recipient))
def read(key, sender, recipient, data):
    assert data[:4] == b'NML4' and len(data) >= 45
    return data[4], open_(key, data[5:29], data[29:], aad(data[4], sender, recipient))

secret = bytes(range(0x20, 0x40))
key = seal_key(secret)
pc, phone = bytes(range(0x10, 0x20)), bytes(range(0xf0, 0x100))      # places: 16 random bytes for each run of an app
n1, n2, n3, n4 = (bytes(range(start, start + 24)) for start in (0xa0, 0xc0, 0x60, 0x40))
look = b'{"t":"look","want":"brief"}'
told = ('{"re":"%s","device":"gYKDhIWGh4iJiouMjY6PkA","name":"Phone","at":[7,12],"playing":true,"title":"Song","artist":"Artist"}' % b64(n1)).encode()
pause = b'{"t":"do","do":"pause","at":[7,12],"with":{}}'
done = ('{"re":"%s","ok":true,"device":"gYKDhIWGh4iJiouMjY6PkA","name":"Phone","at":[8,12],"by":"%s","playing":false,"title":"Song","artist":"Artist"}' % (b64(n3), b64(n3))).encode()
frames = [('PC to phone, look', frame(key, ASK, n1, pc, phone, look), look), ('phone to PC, state', frame(key, TELL, n2, phone, pc, told), told),
          ('PC to phone, pause', frame(key, ASK, n3, pc, phone, pause), pause), ('phone to PC, done', frame(key, TELL, n4, phone, pc, done), done)]
assert read(key, pc, phone, frames[0][1]) == (ASK, look) and read(key, phone, pc, frames[3][1]) == (TELL, done)

def refused(sender, recipient, data, with_key=key):
    try: read(with_key, sender, recipient, data); return False
    except (InvalidTag, AssertionError): return True
first = frames[0][1]
assert refused(phone, pc, first)                                      # handed back to its sender
assert refused(pc, bytes(16), first)                                  # delivered to another place
assert refused(bytes(16), phone, first)                               # the relay names another sender
assert refused(pc, phone, first[:4] + bytes([TELL]) + first[5:])      # an ask passed off as an answer
assert refused(pc, phone, first[:-1])                                 # cut short
assert refused(pc, phone, first, seal_key(bytes(32)))                 # sealed under another secret
assert all(refused(pc, phone, first[:at] + bytes([first[at] ^ 1]) + first[at + 1:]) for at in range(len(first)))  # any bit changed

print('secret   ', secret.hex(), '=', b64(secret))
print('seal key ', key.hex())
print('room     ', room_name(secret))
print('PC place ', pc.hex(), '=', b32(pc)); print('phone    ', phone.hex(), '=', b32(phone))
for name, data, body in frames:
    print(f'\n{name}: {body.decode()}'); print('  nonce  ', data[5:29].hex()); print('  frame  ', data.hex()); print('  as sent', b64(data))

# ---- the design's decisions ---------------------------------------------------------------------
class Device:
    """What a device decides about a command (5.6). `gen` advances with every choice it accepts,
    at the moment the choice is made; `minute` is whole minutes since the app started."""
    def __init__(self): self.gen, self.minute, self.by, self.did = 1, 0, None, []
    def choose(self, what): self.gen += 1; self.by = None; self.did.append(what)
    def state(self): return (self.gen, self.minute)
    def admit(self, at):
        if at[0] != self.gen: return 'changed'
        if self.minute - at[1] not in (0, 1): return 'late'
    def ask(self, nonce, what, at, work=None):
        why = self.admit(at)                       # on arrival, before any work is done for it
        if why: return (False, why, self.state(), self.by)
        self.gen += 1; mine = self.gen             # it is a choice: its place in the order, taken now
        if work: work()
        why = 'changed' if self.gen != mine else 'late' if self.minute - at[1] not in (0, 1) else None   # again, as the player is changed
        if why: return (False, why, self.state(), self.by)
        self.by = nonce; self.did.append(what)
        return (True, None, self.state(), self.by)

def outcome(seen, nonce):
    """What the sender concludes from any state of the other device: one it reads after an
    answer that never came, or one that arrives as a refusal. `by` is believed first."""
    (gen, _), by, sent_at = seen[0], seen[1], seen[2]
    return 'done' if by == nonce else 'not yet' if gen == sent_at[0] else 'not known'

rows = []
def case(name, got, want): assert got == want, (name, got, want); rows.append((name, want))

b = Device(); at = b.state()
ok = b.ask('X', 'next', at)
case('double skip: the frame delivered twice', (ok[0], b.ask('X', 'next', at)[1], b.did), (True, 'changed', ['next']))
again = b.ask('X', 'next', at)
case('double skip: the only answer to arrive is the refusal of the repeat', (again[1], outcome((again[2], again[3], at), 'X')), ('changed', 'done'))
case('double skip: answer lost, the sender reads the state', outcome((b.state(), b.by, at), 'X'), 'done')
case('double skip: a faulty sender asks anew against what it last saw', (b.ask('Y', 'next', at)[1], b.did), ('changed', ['next']))

b = Device(); at = b.state(); b.choose('pause at the device')
case('held Play: the person paused at the device meanwhile', b.ask('P', 'play', at)[:2], (False, 'changed'))
b = Device(); at = b.state(); b.minute += 1
case('held Play: nothing done at the device, delivered a minute on', b.ask('P', 'play', at)[0], True)
b = Device(); at = b.state(); b.minute += 2
case('held Play: delivered two minutes on', (b.ask('P', 'play', at)[:2], b.did), ((False, 'late'), []))
b = Device(); at = b.state()
case('a command admitted in time whose work outlasts it', b.ask('P', 'play', at, lambda: setattr(b, 'minute', 2))[:2], (False, 'late'))

b = Device(); at = b.state(); b.choose('play a list, still being looked up')
case('sequence 14: a held Pause against a Play not yet started', (b.ask('Q', 'pause', at)[:2], b.did), ((False, 'changed'), ['play a list, still being looked up']))
b = Device(); at = b.state()
case('a choice at the device while a command\'s work is under way', b.ask('S', 'add', at, lambda: b.choose('pause at the device'))[:2], (False, 'changed'))

b = Device(); at = b.state()
first, second = b.ask('A1', 'pause', at), b.ask('C1', 'next', at)
case('two devices at the same instant', (first[0], second[1], b.did), (True, 'changed', ['pause']))
b = Device(); at = b.state(); first = b.ask('N', 'next', at); second = b.ask('M', 'pause', first[2])
case('two presses on one device: the second waits for the first\'s answer', (second[0], b.did), (True, ['next', 'pause']))
b = Device(); at = b.state(); b.ask('V', 'volume 20', at); b.choose('volume 60 at the device')
case('an old volume delivered again after a newer one', (b.ask('V', 'volume 20', at)[1], b.did[-1]), ('changed', 'volume 60 at the device'))

b = Device(); at = b.state(); b.ask('N', 'next', at); b.choose('another device adds a song')
case('answer lost, and something else chosen since', outcome((b.state(), b.by, at), 'N'), 'not known')
b = Device(); at = b.state()
case('answer never came because the command never arrived', outcome((b.state(), b.by, at), 'N'), 'not yet')

def move(source_touched=False, destination_plays=True, answer_arrives=True):
    """Moving music from A to B (5.7). Returns what A does and says."""
    a, dest = Device(), Device()
    a.choose('move to B'); began = a.gen
    sent_at = dest.state()
    started = destination_plays and dest.ask('H', 'adopt', sent_at)[0]
    if source_touched: a.choose('another album on A')
    if not answer_arrives: started = outcome((dest.state(), dest.by, sent_at), 'H') == 'done'   # A reads B's state instead
    if not started: return 'A plays on'
    return 'A pauses' if a.gen == began else 'both play; the person is told'
case('moving: the other device cannot play', move(destination_plays=False), 'A plays on')
case('moving: the answer is lost', move(answer_arrives=False), 'A pauses')
case('moving: the person starts something else on A before the answer', move(source_touched=True), 'both play; the person is told')

def take(source_touched=False):
    """A takes B's queue ("Play here"): A starts, then asks B to pause against what it read."""
    source = Device(); read_at = source.state()
    if source_touched: source.choose('another album on B')
    return 'B pauses' if source.ask('T', 'pause', read_at)[0] else 'both play; the person is told'
case('taking: nothing done on B meanwhile', take(), 'B pauses')
case('taking: B was given something else meanwhile', take(source_touched=True), 'both play; the person is told')

restarted = bytes(range(0x30, 0x40))               # the app restarted: a new place, and a count that starts again
case('the receiving device restarted: the Pause sealed for its old place', 'does not open' if refused(pc, restarted, frames[2][1]) else 'opens', 'does not open')

print()
for name, want in rows: print(f'{name:68} {want}')
