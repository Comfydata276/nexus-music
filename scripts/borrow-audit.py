#!/usr/bin/env python3
"""Finds a RefCell of the controller borrowed twice at once, before a person does.

    python3 scripts/borrow-audit.py              the app's source (src/), from the repository's root
    python3 scripts/borrow-audit.py --self-test  the script itself, on the shape it exists for

Run by scripts/check, and so by the release and by the check on GitHub.

The fault. The app's state is in `RefCell`s on the controller. A borrow taken in the head
of a `match` is held until the match ends; if an arm then calls something that borrows the
same cell for writing, the app stops there, and the released app is built to close when
that happens (`panic = "abort"`). 0.40.9 and every version before it did exactly this when
a linked device changed another PC's volume, or a guest allowed to steer pressed Shuffle or
Repeat: the request's handler read a setting in the head of the match that carried the
request out, and those arms write the settings. Nothing in the compiler or the tests saw
it, because it only happens when the request arrives.

What this looks at: every place a cell of the controller is borrowed while a method of the
controller is called, in three shapes:

  A  a borrow in the head of a `match`, `if let`, `while let` or `for`
     (a temporary there lives to the end of the whole statement);
  B  a borrow bound by `let` (alive to the end of its block, or to `drop`);
  C  a borrow and a call in one statement (the temporary lives to the statement's end).

For each, it works out what the methods called reach in the same cell, to any depth,
leaving out what runs later (a closure posted to the UI thread, or handed to a timer, a
pool, a request or a callback of the window). It is a conflict if the cell is held for
writing and a call reaches it at all, or held for reading and a call writes it.

Its two limits:

  1. It follows a cell only when it is reached as `self.<cell>`, `ctl.<cell>`,
     `this.<cell>` or `controller.<cell>`. A cell reached by another name (a reference
     kept in a variable, a cell inside another struct's own state, as `link.rs` has) is not
     seen: such code has to be read.
  2. In shape C it reads a statement of at most twelve lines. A longer one is not seen.

It reads the source as text, so it is quick and needs no build; and for the same reason
it can be wrong. Where it reports something that is not a fault, change the code so that
the borrow ends before the call (read the value into a `let` of its own) rather than
teach the script an exception: that is the fix for the real fault too.
"""
import glob
import os
import re
import sys
import tempfile

RECEIVER = r'\b(?:self|ctl|this|controller)'
ACCESS = re.compile(RECEIVER + r'\.(\w+)(?:\.\w+)*?\.(borrow_mut|borrow|replace|take)\(')
CALL = re.compile(RECEIVER + r'\.(\w+)\(')
HEAD = re.compile(r'\b(match|if let|while let|for)\b')
BOUND = re.compile(r'^(\s*)let (?:mut )?(\w+)(?:: [^=]+)? = (?:&mut \*|&\*|&)?' + RECEIVER[2:] + r'\.(\w+)[\w.]*?\.(borrow|borrow_mut)\(\);\s*(?://.*)?$')
TEMPORARY = re.compile(RECEIVER + r'\.(\w+)(?:\.\w+)*?\.(borrow_mut|borrow)\(\)')
LATER = re.compile(r'(?:\bpost|\.start|\bspawn|\.spawn|\.request|\.on_\w+|invoke_from_event_loop|single_shot)\(')
MOST_LINES = 12


def now_only(text):
    """The text without what is handed to something that runs it later."""
    out, at = [], 0
    while True:
        found = LATER.search(text, at)
        if not found:
            out.append(text[at:])
            return ''.join(out)
        out.append(text[at:found.end()])
        depth, end = 1, found.end()
        while end < len(text) and depth:
            depth += (text[end] == '(') - (text[end] == ')')
            end += 1
        out.append(')')
        at = end


def block(lines, first):
    """The lines from `first` to the end of the block that opens there."""
    depth, started, out = 0, False, []
    for line in lines[first:]:
        out.append(line)
        depth += line.count('{') - line.count('}')
        started = started or '{' in line
        if started and depth <= 0:
            break
    return out


class Source:
    def __init__(self, root):
        self.files = {}
        for path in sorted(glob.glob(os.path.join(root, 'src', '**', '*.rs'), recursive=True)):
            lines = open(path, encoding='utf-8').read().split('\n')
            # (Tests are not the app.)
            end = next((n for n, line in enumerate(lines) if line.strip() == '#[cfg(test)]'), len(lines))
            self.files[os.path.relpath(path, root).replace(os.sep, '/')] = lines[:end]
        self.bodies = {}
        for lines in self.files.values():
            for n, line in enumerate(lines):
                named = re.match(r'\s*(?:pub(?:\([a-z]+\))? )?fn (\w+)', line)
                if named:
                    self.bodies.setdefault(named.group(1), []).append('\n'.join(block(lines, n)[1:]))
        self.reached = {}

    def reach(self, method, under=frozenset()):
        """Each cell the method touches, itself or through what it calls: 'w' or 'r'."""
        if method in self.reached:
            return self.reached[method]
        if method in under or method not in self.bodies:
            return {}
        out = {}
        for body in self.bodies[method]:
            body = now_only(body)
            for cell, how in ACCESS.findall(body):
                out[cell] = 'w' if how != 'borrow' or out.get(cell) == 'w' else 'r'
            for called in CALL.findall(body):
                for cell, how in self.reach(called, under | {method}).items():
                    out[cell] = 'w' if how == 'w' or out.get(cell) == 'w' else 'r'
        if not under:
            self.reached[method] = out
        return out

    def conflicts(self, cell, held, text):
        found = set()
        for called in CALL.findall(now_only(text)):
            how = self.reach(called).get(cell)
            if how and (held == 'borrow_mut' or how == 'w'):
                found.add(f'{called}() {"writes" if how == "w" else "reads"} it')
        return sorted(found)

    def audit(self):
        found = []
        for path, lines in self.files.items():
            for n, line in enumerate(lines):
                if line.strip().startswith('//'):
                    continue
                where = f'{path}:{n + 1}'
                holding = lambda how: 'writing' if how == 'borrow_mut' else 'reading'
                # A: in the head of a match, if let, while let or for.
                for head_at in HEAD.finditer(line):
                    last, head = n, line[head_at.start():]
                    while not head.rstrip().endswith('{') and last + 1 < len(lines) and last - n < 8:
                        last += 1
                        head += ' ' + lines[last].strip()
                    if head_at.group(1) == 'for' and ' in ' not in head:
                        continue
                    held = TEMPORARY.findall(head)
                    if held:
                        body = '\n'.join(block(lines, last))
                        for cell, how in held:
                            for conflict in self.conflicts(cell, how, body):
                                found.append(f'{where}: `{cell}` is held for {holding(how)} by the head of this `{head_at.group(1)}`, to its end: {conflict}')
                        break
                # B: bound by let.
                bound = BOUND.match(line)
                if bound:
                    indent, name, cell, how = len(bound.group(1)), bound.group(2), bound.group(3), bound.group(4)
                    alive = []
                    for after in lines[n + 1:]:
                        if after.strip() and len(after) - len(after.lstrip()) < indent:
                            break
                        if re.search(rf'\bdrop\({name}\)', after):
                            break
                        if not after.strip().startswith('//'):
                            alive.append(after)
                    for conflict in self.conflicts(cell, how, '\n'.join(alive)):
                        found.append(f'{where}: `{cell}` is held for {holding(how)} as `{name}`, to the end of its block: {conflict}')
                    continue
                # C: in one statement.
                if TEMPORARY.search(line):
                    last, statement = n, line
                    while not re.search(r'[;{}]\s*(//.*)?$', statement) and last + 1 < len(lines) and last - n < MOST_LINES:
                        last += 1
                        statement += '\n' + lines[last]
                    for cell, how in sorted(set(TEMPORARY.findall(statement))):
                        for conflict in self.conflicts(cell, how, statement):
                            found.append(f'{where}: `{cell}` is borrowed for {holding(how)} in a statement that also calls it: {conflict}')
        return sorted(set(found))


# ---- the script's own test --------------------------------------------------------------------

# The shape 0.40.9 had, cut down: a request is carried out by a match whose head reads the
# settings, and whose arms reach, two calls down, something that writes them.
AS_IT_WAS = '''
impl Controller {
    fn jam_asked(self: &Rc<Self>, action: &str, data: &Value) {
        let done = match answer(session, own, self.settings.borrow().jam_guests_control, action) {
            Answer::No(why) => Err(why.to_string()),
            Answer::Do { steer } => match action {
                "volume" => {
                    self.set_volume(data["to"].as_f64().unwrap_or(1.0) as f32);
                    Ok(json!({}))
                }
                "shuffle" | "repeat" => self.jam_control(action, data).map(|()| json!({})),
                _ => Err(NOT_FOR_GUESTS.to_string()),
            },
        };
    }

    fn jam_control(self: &Rc<Self>, action: &str, data: &Value) -> Result<(), String> {
        match action {
            "shuffle" => self.set_shuffle(data["on"].as_bool() == Some(true)),
            _ => self.set_repeat(0),
        }
        Ok(())
    }

    pub fn set_volume(self: &Rc<Self>, volume: f32) {
        self.settings.borrow_mut().volume = volume;
        self.save_settings_soon();
    }

    pub fn set_shuffle(self: &Rc<Self>, on: bool) {
        self.settings.borrow_mut().shuffle = on;
    }

    pub fn set_repeat(self: &Rc<Self>, mode: u8) {
        self.settings.borrow_mut().repeat = mode;
    }

    fn save_settings_soon(&self) {
        // (What a timer runs later borrows nothing now.)
        self.save_timer.start(slint::TimerMode::SingleShot, SETTLE, || {
            with(|ctl| ctl.settings.borrow_mut().saved = true);
        });
    }
}
'''
# As it was mended: the setting is read into a value of its own first.
AS_MENDED = AS_IT_WAS.replace(
    'let done = match answer(session, own, self.settings.borrow().jam_guests_control, action) {',
    'let guests_control = self.settings.borrow().jam_guests_control;\n        let done = match answer(session, own, guests_control, action) {',
)
# The other two shapes, each with one fault and one place that is no fault.
OTHER_SHAPES = '''
impl Controller {
    fn bound(&self) {
        let pb = self.playback.borrow();
        self.sync_transport();
        self.stop();
    }

    fn bound_and_dropped(&self) {
        let pb = self.playback.borrow();
        let playing = pb.playing;
        drop(pb);
        self.stop();
    }

    fn one_statement(&self) {
        self.show(self.playback.borrow().position, self.stop());
    }

    fn read_twice(&self) {
        self.show(self.playback.borrow().position, self.sync_transport());
    }

    fn held_for_writing(&self) {
        if let Some(song) = self.playback.borrow_mut().current.as_mut() {
            self.sync_transport();
        }
    }

    fn stop(&self) {
        self.playback.borrow_mut().playing = false;
    }

    fn sync_transport(&self) {
        let playing = self.playback.borrow().playing;
    }

    fn show(&self, position: f64, _: ()) {}
}
'''


def self_test():
    def report(text):
        with tempfile.TemporaryDirectory() as root:
            os.makedirs(os.path.join(root, 'src'))
            open(os.path.join(root, 'src', 'sample.rs'), 'w', encoding='utf-8').write(text)
            return Source(root).audit()

    wanted = [
        (AS_IT_WAS, ['src/sample.rs:4: `settings` is held for reading by the head of this `match`, to its end: jam_control() writes it',
                     'src/sample.rs:4: `settings` is held for reading by the head of this `match`, to its end: set_volume() writes it']),
        (AS_MENDED, []),
        (OTHER_SHAPES, ['src/sample.rs:17: `playback` is borrowed for reading in a statement that also calls it: stop() writes it',
                        'src/sample.rs:25: `playback` is held for writing by the head of this `if let`, to its end: sync_transport() reads it',
                        'src/sample.rs:4: `playback` is held for reading as `pb`, to the end of its block: stop() writes it']),
    ]
    failed = False
    for text, expected in wanted:
        got = report(text)
        if got != sorted(expected):
            failed = True
            print('borrow-audit: the script did not report what it should have. Expected:', *sorted(expected), 'and got:', *got, sep='\n  ')
    if not failed:
        print('borrow-audit: the script reports the fault 0.40.9 had, each of its three shapes, and nothing where there is none')
    return 1 if failed else 0


if __name__ == '__main__':
    if '--self-test' in sys.argv:
        sys.exit(self_test())
    root = next((arg for arg in sys.argv[1:] if not arg.startswith('-')), os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
    found = Source(root).audit()
    for line in found:
        print(line)
    if found:
        print(f'borrow-audit: {len(found)} place{"s" if len(found) != 1 else ""} where a cell is borrowed twice at once (see the top of scripts/borrow-audit.py). End the first borrow before the call: read what is needed into a `let` of its own.')
        sys.exit(1)
    print('borrow-audit: no cell of the controller is borrowed twice at once, as far as this can see')
