#!/usr/bin/env python3
"""Differential fuzzer for date strings: Dogfish `date -d` vs GNU `date`.

  date-fuzz.py [--minutes N | --seconds N] [--jobs N] [--seed S]
               [--dogfish PATH] [--gnu-date PATH] [--gnu-touch PATH]
               [--gnu-tree DIR] [--update-seeds] [--out FILE] [--stop-after N]

Generates date strings from a grammar modelled on gnulib's parse-datetime.y
(dates, times, zones, relative items, days of the week, @epoch, TZ="..."
prefixes, hybrid numbers, the stamps date itself prints, the lexer's and
grammar's edge cases, junk) and from mutations of a seed corpus: the strings of
GNU's own date and touch tests and gnulib's test-parse-datetime.c (saved in
util/date-fuzz-seeds.json; --update-seeds re-extracts them from a GNU source
tree) and those of Dogfish's differential date suite.

Each case runs one batch of strings through both implementations, as
`date [--debug] [-u] -f - FORMAT` (many strings, one process each side) or
`date [--debug] -d STRING FORMAT` (one string), under one of several TZ values,
and compares stdout, stderr and the exit status byte for byte. A smaller share
of cases runs `touch -d STRING f` and compares `stat -c '%x|%y' f`, touch's
stderr and exit status.

Both sides see the same "now": with libfaketime installed, every case pins the
clock (FAKETIME=EPOCH, a frozen clock) to a chosen instant, so strings
like "next monday" or "noon" are compared too. Without it, every generated
string names an absolute date, so no output depends on the clock.

A diverging batch is narrowed to the line that differs alone (or, when lines
only differ together, to the fewest lines that still do), then re-run twice
to make sure it reproduces; one that does not is counted as flaky, not
reported. (libfaketime's "@EPOCH i0.0" mode, unlike the frozen mode used here,
shows the real clock to a process whose PID has a stale semaphore in /dev/shm.)
Divergences are printed with a command that reproduces them and, with --out,
appended to FILE as JSON lines. The exit status is 1 if any case diverged.
"""

import argparse
import glob
import json
import multiprocessing
import os
import random
import re
import shlex
import subprocess
import sys
import shutil
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

TZS = [
    "UTC0",
    "UTC",
    "America/New_York",
    "Europe/London",
    "Asia/Kolkata",
    "Australia/Lord_Howe",
    "Pacific/Chatham",
    "EST5EDT,M3.2.0,M11.1.0",
    # Less common: southern-hemisphere DST, a negative DST, odd POSIX strings.
    "America/Sao_Paulo",
    "Europe/Dublin",
    "Europe/Moscow",  # 2014: a repeated hour with the same tm_isdst
    "Asia/Tokyo",
    "Antarctica/Troll",
    "<+0330>-3:30",
    "XXX-14",
    "NZST-12NZDT,M9.5.0,M4.1.0/3",
    "Factory",
    "",
]
# The main zones get most of the cases.
TZ_WEIGHTS = [6, 2, 6, 6, 6, 6, 6, 6, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]

FORMATS = [
    ["+%Y-%m-%d %H:%M:%S.%N %z %Z"],
    ["+%s.%N"],
    ["+%F %T %:z %::z %:::z %Z %a %A %b %j %U %W %V %G %u %w %C %y %p %I %l"],
    ["-R"],
    ["--rfc-3339=ns"],
    ["-Ins"],
    ["-Iseconds"],
    ["-Ihours"],
    ["+%c|%x|%X|%r|%P|%q|%k|%e|%h|%3N|%-d|%_m|%^a|%#Z|%10s|%+4Y|%g"],
    [],
]

# Instants for the frozen clock: epoch boundaries, leap days, DST changes in
# the zones above, and the y2038 boundary.
NOWS = [
    # Not 0: libfaketime takes a clock of exactly 0 as unset in some calls.
    "1",
    "86399.999999999",
    "951782400",  # 2000-02-29 00:00 UTC
    "946684799.5",  # 1999-12-31 23:59:59.5 UTC
    "1583650799",  # 2020-03-08 06:59:59 UTC, just before New York DST
    "1583650800",
    "1604208600",  # 2020-11-01 05:30 UTC, New York's repeated hour
    "1414272600",  # 2014-10-25 21:30 UTC, Moscow's repeated hour
    "1616893200",  # 2021-03-28 01:00 UTC, London DST
    "1635642000",  # 2021-10-31 01:00 UTC
    "1601739000",  # 2020-10-03 15:30 UTC, Lord Howe +30 min DST
    "1586010600",  # 2020-04-04 14:30 UTC
    "1632579300",  # 2021-09-25 14:15 UTC, Chatham DST
    "1709251199",  # 2024-02-29 23:59:59 UTC
    "1700000000.123456789",
    "2147483647",
    "2147483648",
]

UNITS = ["YEAR", "MONTH", "FORTNIGHT", "WEEK", "DAY", "HOUR", "MINUTE", "MIN",
         "SECOND", "SEC"]
SHIFTS = ["TOMORROW", "YESTERDAY", "TODAY", "NOW"]
ORDINALS = ["LAST", "THIS", "NEXT", "FIRST", "THIRD", "FOURTH", "FIFTH",
            "SIXTH", "SEVENTH", "EIGHTH", "NINTH", "TENTH", "ELEVENTH",
            "TWELFTH"]
# "second" is also a unit, so it is no ordinal word.
MONTHS = ["JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY",
          "AUGUST", "SEPTEMBER", "SEPT", "OCTOBER", "NOVEMBER", "DECEMBER"]
DAYS = ["SUNDAY", "MONDAY", "TUESDAY", "TUES", "WEDNESDAY", "WEDNES",
        "THURSDAY", "THUR", "THURS", "FRIDAY", "SATURDAY"]
MERIDIANS = ["AM", "A.M.", "PM", "P.M.", "am", "pm", "a.m.", "p.m.", "Am"]
# gnulib's universal and time_zone_table entries.
ZONES = ["GMT", "UT", "UTC", "WET", "WEST", "BST", "ART", "BRT", "BRST", "NST",
         "NDT", "AST", "ADT", "CLT", "CLST", "EST", "EDT", "CST", "CDT", "MST",
         "MDT", "PST", "PDT", "AKST", "AKDT", "HST", "HAST", "HADT", "SST",
         "WAT", "CET", "CEST", "MET", "MEZ", "MEST", "MESZ", "EET", "EEST",
         "CAT", "SAST", "EAT", "MSK", "MSD", "IST", "SGT", "KST", "JST", "GST",
         "NZST", "NZDT"]
# Abbreviations the TZ values above use locally (tm_zone), and near misses.
LOCAL_ZONES = ["EST", "EDT", "GMT", "BST", "IST", "LMT", "+1030", "+11",
               "+1245", "+1345", "-03", "-02", "JST", "+00", "+02", "-00",
               "NZST", "NZDT", "XXX", "UTC", "CHADT", "LHDT", "EDT5", "E.S.T.",
               "e.d.t."]
MILITARY = [chr(c) for c in range(ord("A"), ord("Z") + 1)]
WORDS = ["DST", "AGO", "HENCE", "NOON", "MIDNIGHT", "AT", "ON", "OF", "AND",
         "T", "Z", "J", "UTC", "noon", "midnight", "foo", "x", "TZ"]
TZ_STRINGS = [
    "UTC0", "UTC", "", "Asia/Tokyo", "America/New_York", "Europe/London",
    "Asia/Kolkata", "Australia/Lord_Howe", "Pacific/Chatham",
    "America/Edmonton", "EST5", "EST5EDT", "EST5EDT,M3.2.0,M11.1.0",
    "PST8PDT,M3.2.0/2:00:00,M11.1.0/2:00:00", "NZST-12NZDT,M9.5.0,M4.1.0/3",
    "EST5EDT,J60/2,J300/2", "XXX-3:30", "<+0330>-3:30", "<-03>3", "XXX24",
    "XXX-24:59:59", "XXX25", "GMT+12", "UTC-14", ":America/New_York",
    ":UTC", "/usr/share/zoneinfo/Asia/Kolkata", "Foo/Bar", "UTC+25", "<abc",
    "a\\\"b", "a\\\\b", "a\\b", "\\", "EST5\\", "Factory", "-00",
    "America/Sao_Paulo", "Antarctica/Troll", "../../etc/localtime", "posixrules",
    "A" * 300 + "0",
]
TIME_SEPS = [":", ":", ":", "."]
FRACTION_DIGITS = "0123456789"

# Raw lexer-level characters, including bytes that are not ASCII.
JUNK = list("()[]{}<>?!#$%^&*~`'\";|=_-+,./:@\\ \t\x0b\x0c\r") + [
    "\x01", "\x7f", "\xa0", "\xb0", "\xe9", "\xff", "é", " ", "\U0001f600"]


class Gen:
    """Grammar-based generator of date strings (as bytes)."""

    def __init__(self, rng, seeds, pinned_now):
        self.r = rng
        self.seeds = seeds
        self.pinned_now = pinned_now

    def chance(self, p):
        return self.r.random() < p

    def pick(self, items):
        return items[self.r.randrange(len(items))]

    def case(self, word):
        """WORD in a random case, sometimes abbreviated to 3 letters with a dot."""
        r = self.r.random()
        if r < 0.15 and len(word) > 3 and word.isalpha():
            word = word[:3] + ("." if self.chance(0.3) else "")
        r = self.r.random()
        if r < 0.4:
            return word.lower()
        if r < 0.7:
            return word.capitalize()
        if r < 0.9:
            return word
        return "".join(c.upper() if self.chance(0.5) else c.lower() for c in word)

    # Numbers --------------------------------------------------------------

    def digits(self, n):
        return "".join(self.pick(FRACTION_DIGITS) for _ in range(n))

    def num(self):
        k = self.r.randrange(14)
        if k < 4:
            return str(self.r.randrange(100)).zfill(self.r.randrange(1, 3))
        if k < 6:
            return str(self.pick([0, 1, 12, 13, 23, 24, 25, 29, 30, 31, 32, 59, 60, 61, 99, 100]))
        if k < 8:
            return str(self.r.randrange(10000)).zfill(self.r.randrange(3, 5))
        if k == 8:
            return self.digits(self.r.randrange(5, 9))
        if k == 9:
            return self.digits(self.r.randrange(9, 13))
        if k == 10:
            return self.pick(["2147483647", "2147483648", "4294967295", "4294967296",
                              "9223372036854775807", "9223372036854775808",
                              "18446744073709551615", "99999999999999999999",
                              "2147485547", "2147485548", "67768036191676799",
                              "67768036191676800"])
        if k == 11:
            return self.digits(self.r.randrange(13, 30))
        if k == 12:
            return "0" * self.r.randrange(1, 12) + str(self.r.randrange(100))
        return str(self.r.randrange(61))

    def frac(self):
        sep = "." if self.chance(0.8) else ","
        n = self.pick([1, 1, 2, 3, 6, 9, 9, 10, 12, 20])
        return sep + self.digits(n)

    def sign(self):
        s = self.pick(["+", "-", "+", "-", "- ", "+ ", "--", "+-", "-  "])
        return s

    def year(self):
        return self.pick(["69", "70", "68", "99", "00", "0", "1", "100", "999",
                          "1000", "1900", "1969", "1970", "1971", "1999", "2000",
                          "2020", "2024", "2037", "2038", "2039", "2106", "9999",
                          "10000", "2147483647", "2147485547", "2147485548",
                          "-1", "0000", "0005", "292277026596", "292277026597",
                          str(self.r.randrange(1900, 2100)),
                          str(self.r.randrange(100)).zfill(2)])

    def month(self):
        return str(self.pick([1, 2, 3, 6, 10, 11, 12, 0, 13, self.r.randrange(1, 13)])).zfill(
            self.r.randrange(1, 3))

    def mday(self):
        return str(self.pick([1, 28, 29, 30, 31, 0, 32, self.r.randrange(1, 32)])).zfill(
            self.r.randrange(1, 3))

    # Grammar items --------------------------------------------------------

    def date(self):
        y, m, d = self.year(), self.month(), self.mday()
        mon = self.case(self.pick(MONTHS))
        k = self.r.randrange(13)
        if k < 4:
            return f"{y}-{m}-{d}"
        if k == 4:
            return f"{m}/{d}"
        if k == 5:
            return f"{m}/{d}/{y}"
        if k == 6:
            return f"{y}/{m}/{d}"
        if k == 7:
            return f"{d}-{mon}-{y}"
        if k == 8:
            return f"{mon}-{d}-{y}"
        if k == 9:
            return self.pick([f"{mon} {d}", f"{mon} {d}, {y}", f"{mon} {d} {y}", f"{mon}{d}"])
        if k == 10:
            return self.pick([f"{d} {mon}", f"{d} {mon} {y}", f"{d}{mon}{y}", f"{mon} {y}"])
        if k == 11:
            return self.pick([f"{y}{m.zfill(2)}{d.zfill(2)}", f"{y}-{m}", f"{y}{m.zfill(2)}"])
        return f"{y}-{m}-{d}T{self.time(meridian=False)}"

    def time(self, meridian=None):
        h = str(self.pick([0, 1, 9, 11, 12, 13, 23, 24, 25, self.r.randrange(24)])).zfill(
            self.r.randrange(1, 3))
        mi = str(self.pick([0, 30, 59, 60, self.r.randrange(60)])).zfill(self.r.randrange(1, 3))
        s = str(self.pick([0, 59, 60, 61, self.r.randrange(60)])).zfill(self.r.randrange(1, 3))
        k = self.r.randrange(6)
        if k == 0:
            t = h
            meridian = True
        elif k < 3:
            t = f"{h}:{mi}"
        elif k < 5:
            t = f"{h}:{mi}:{s}"
        else:
            t = f"{h}:{mi}:{s}{self.frac()}"
        if meridian is None:
            meridian = self.chance(0.25)
        if meridian:
            t += self.pick(["", " "]) + self.pick(MERIDIANS)
        elif self.chance(0.2):
            t += self.pick(["", " "]) + self.offset()
        return t

    def offset(self):
        s = self.pick(["+", "-"])
        k = self.r.randrange(7)
        if k == 0:
            return s + str(self.r.randrange(25)).zfill(2)
        if k == 1:
            return s + str(self.r.randrange(25)).zfill(2) + str(self.r.randrange(61)).zfill(2)
        if k == 2:
            return s + str(self.r.randrange(25)) + ":" + str(self.r.randrange(61)).zfill(2)
        if k == 3:
            return s + self.pick(["0", "00", "0000", "2400", "2401", "24:00", "1:3",
                                  "1:300", "12345", "99", "100", "0530", "5:30",
                                  "13:45", "1245"])
        if k == 4:
            return s + str(self.r.randrange(10))
        if k == 5:
            return s + self.digits(self.r.randrange(1, 7))
        return s + self.num()

    def zone(self):
        k = self.r.randrange(10)
        if k < 3:
            z = self.case(self.pick(ZONES))
        elif k < 5:
            z = self.pick(MILITARY)
            if self.chance(0.5):
                z = z.lower()
        elif k == 5:
            z = self.pick(LOCAL_ZONES)
        elif k < 8:
            z = self.pick(["UTC", "GMT", "UT", "utc", "Z", "EST", "J", "T"]) + self.offset()
        else:
            z = self.offset()
        if self.chance(0.1):
            z += self.pick([" DST", "DST", " dst"])
        return z

    def rel(self):
        unit = self.case(self.pick(UNITS))
        if self.chance(0.3):
            unit += "S" if unit.isupper() else "s"
        k = self.r.randrange(9)
        if k < 3:
            n = self.num() if self.chance(0.7) else str(self.r.randrange(10))
            if self.chance(0.6):
                n = self.sign() + n
            if unit.lower().startswith("sec") and self.chance(0.4):
                n += self.frac()
            r = f"{n}{self.pick([' ', '', '  '])}{unit}"
        elif k == 3:
            r = unit
        elif k < 6:
            r = f"{self.case(self.pick(ORDINALS))} {unit}"
        elif k == 6:
            r = self.case(self.pick(SHIFTS))
        elif k == 7:
            r = f"{self.sign()}{self.r.randrange(100)} {unit}"
        else:
            r = f"{self.case(self.pick(['second', 'SECOND']))} {unit}"
        if self.chance(0.25):
            r += " " + self.case(self.pick(["AGO", "HENCE"]))
        return r

    def day(self):
        d = self.case(self.pick(DAYS))
        k = self.r.randrange(6)
        if k == 0:
            return d + ","
        if k == 1:
            return f"{self.case(self.pick(ORDINALS))} {d}"
        if k == 2:
            return f"{self.sign() if self.chance(0.3) else ''}{self.r.randrange(13)} {d}"
        return d

    def hybrid(self):
        """A bare number: an hour, HHMM, a year, YYYYMMDD, YYYYMMDDHHMM..."""
        return self.pick([
            str(self.r.randrange(25)),
            str(self.r.randrange(2400)).zfill(4),
            self.year(),
            f"{self.r.randrange(1900, 2100)}{self.month().zfill(2)}{self.mday().zfill(2)}",
            self.digits(self.r.randrange(1, 15)),
            self.sign() + self.digits(self.r.randrange(1, 5)),
        ])

    def epoch(self):
        n = self.pick(["0", "1", "-1", self.num(), self.sign().strip() + self.num(),
                       "9223372036854775807", "-9223372036854775808",
                       "67768036191676799", "67768036191676800",
                       "-67768040609740800", "-67768040609740801",
                       str(self.r.randrange(-2**34, 2**34))])
        if self.chance(0.4):
            n += self.frac()
        return "@" + self.pick(["", "", "", " "]) + n

    def tz_prefix(self):
        tz = self.pick(TZ_STRINGS)
        if self.chance(0.1):
            tz = self.mutate_chars(tz) if tz else tz
        k = self.r.randrange(20)
        if k == 0:
            return f'TZ="{tz}'  # unterminated
        if k == 1:
            return f"TZ={tz} "
        if k == 2:
            return f'tz="{tz}" '
        if k == 3:
            return f'  TZ="{tz}"'
        return f'TZ="{tz}"' + self.pick([" ", "", "  ", "\t"])

    def comment(self):
        return self.pick(["(comment)", "(a (nested) one)", "(unterminated", "()", ")", "((x)"])

    def junk(self):
        return "".join(self.pick(JUNK) for _ in range(self.r.randrange(1, 4)))

    def item(self):
        k = self.r.randrange(100)
        if k < 11:
            return self.date()
        if k < 14:
            return self.pick([self.rfc, self.boundary])()
        if k < 28:
            return self.time()
        if k < 38:
            return self.zone()
        if k < 58:
            return self.rel()
        if k < 66:
            return self.day()
        if k < 74:
            return self.hybrid()
        if k < 78:
            return self.epoch()
        if k < 82:
            return self.case(self.pick(WORDS))
        if k < 86:
            return self.comment()
        if k < 90:
            return self.pick(["T", "t", "Z", ",", ".", "+", "-", ":", "/", "@"])
        if k < 95:
            return self.num()
        return self.junk()

    def rfc(self):
        """Whole stamps in the shapes date itself prints: -R, --rfc-3339, -I, ctime."""
        y, m, d = self.year(), self.month().zfill(2), self.mday().zfill(2)
        t = self.time(meridian=False)
        day = self.case(self.pick(DAYS))[:3]
        mon = self.case(self.pick(MONTHS))[:3]
        return self.pick([
            f"{day}, {d} {mon} {y} {t} {self.offset()}",
            f"{y}-{m}-{d} {t}{self.offset()}",
            f"{y}-{m}-{d}T{t}{self.pick(['Z', 'z', self.offset(), ''])}",
            f"{y}-{m}-{d}T{self.r.randrange(25):02}{self.offset()}",
            f"{day} {mon} {d.lstrip('0') or '0':>2} {t} {self.pick(ZONES + MILITARY)} {y}",
            f"{day} {mon} {d} {t} {y}",
        ])

    def boundary(self):
        """Inputs at the edges of the lexer and the Bison grammar."""
        n = self.digits(self.r.randrange(1, 16))
        return self.pick([
            n,  # 1-2 digits an hour, 3-4 HHMM, 5-8 a date, more a date and HHMM
            f"{n}{self.case(self.pick(MONTHS))}",  # "17jun": number then month
            f"{self.case(self.pick(MONTHS))}{n}",
            f"{n}:{self.digits(2)}{self.offset()}",  # a time with an offset
            f"{n}.{self.digits(self.r.randrange(0, 12))}",  # a number then '.'
            f"{self.sign()}{n}{self.pick(['', ' '])}{self.case(self.pick(UNITS))}",
            f"{self.digits(4)}-{self.digits(2)}-{self.digits(2)}T{self.digits(2)}",
            f"{self.digits(4)}-{self.digits(2)}-{self.digits(2)}{self.pick(['T', 't', ' T', 'T '])}",
            f"{self.digits(2)}:{self.digits(2)}{self.pick(MILITARY)}",  # "09:00T"
            f"{self.digits(2)}:{self.digits(2)}:{self.digits(2)}{self.frac()}{self.pick(['', ' '])}"
            f"{self.pick(MERIDIANS)}",
            f"@{self.sign().strip()}{n}{self.frac() if self.chance(0.5) else ''}",
            f"{self.case(self.pick(ZONES))}{self.sign().strip()}{n}",  # zone + hhmm
            f"{self.case(self.pick(ZONES))} {self.case('DST')}",
            f"{self.case(self.pick(ORDINALS))} {self.case(self.pick(DAYS))}",
            f"{self.case(self.pick(ORDINALS))}{self.pick(['', ' ', '  '])}{self.case(self.pick(UNITS))}",
            f"{n} {self.case(self.pick(['AGO', 'HENCE']))}",
            f"{self.sign()}{self.sign()}{n}",
            # Repeated and skipped local hours of the zones above.
            self.pick(["2014-10-26 01:30", "2020-11-01 01:30", "2020-03-08 02:30",
                       "2021-03-28 01:30", "2021-10-31 01:30", "2020-04-05 01:45",
                       "2020-10-04 02:15", "2021-09-26 02:50", "2021-04-04 02:50",
                       "1990-01-01 00:00", "2005-02-12 01:00"]),
            self.pick(["0", "00", "000", "0000", "00000", "000000", "0000000", "00000000",
                       "-0", "+0", "0x10", "1e5", "1,5", "1.5.5", "12:", ":12", "12::30",
                       "1:2:3:4", "12:30:", "24:00", "24:00:00", "23:59:60", "T", "TT",
                       "J", "JJ", "Z Z", "DST DST", "ago ago", "@@1", "TZ=", 'TZ=""',
                       'TZ="', '"', "\\\\", "\\"]),
        ])

    def compose(self):
        items = [self.item() for _ in range(self.pick([0, 1, 1, 2, 2, 3, 3, 4, 5, 7]))]
        s = ""
        for it in items:
            s += it + self.pick([" ", " ", " ", "", "  ", ",", "\t", ", "])
        if self.chance(0.5):
            s = s.rstrip()
        if self.chance(0.12):
            s = self.tz_prefix() + s
        return s

    # Mutation -------------------------------------------------------------

    def mutate_chars(self, s):
        s = list(s)
        for _ in range(self.r.randrange(1, 4)):
            op = self.r.randrange(6)
            i = self.r.randrange(len(s) + 1)
            if op == 0 or not s:
                s.insert(i, self.pick(JUNK + list("0123456789Tt:")))
            elif op == 1:
                del s[min(i, len(s) - 1)]
            elif op == 2:
                j = min(i, len(s) - 1)
                s.insert(j, s[j])
            elif op == 3:
                j = min(i, len(s) - 1)
                c = s[j]
                s[j] = c.swapcase() if c.isalpha() else (
                    str((int(c) + self.r.choice([1, 9])) % 10) if c.isdigit() else c)
            elif op == 4:
                j = min(i, len(s) - 1)
                s[j] = self.pick(list("0123456789"))
            else:
                s.insert(i, self.pick([" ", "-", "+", "."]))
        return "".join(s)

    def mutate_tokens(self, s):
        toks = s.split(" ")
        for _ in range(self.r.randrange(1, 3)):
            op = self.r.randrange(4)
            i = self.r.randrange(len(toks) + 1)
            if op == 0:
                toks.insert(i, self.item())
            elif op == 1 and len(toks) > 1:
                del toks[min(i, len(toks) - 1)]
            elif op == 2:
                toks[min(i, len(toks) - 1)] = self.item()
            else:
                self.r.shuffle(toks)
        return " ".join(toks)

    def string(self):
        k = self.r.randrange(10)
        if k < 2 and self.seeds:
            s = self.pick(self.seeds)
        elif k < 4 and self.seeds:
            s = self.mutate_tokens(self.pick(self.seeds))
        elif k < 5 and self.seeds:
            s = self.mutate_chars(self.pick(self.seeds))
        elif k < 6:
            s = self.mutate_tokens(self.compose())
        else:
            s = self.compose()
        if not self.pinned_now and not re.search(r"\d{4}-\d|@\s*-?\d", s):
            # Without a frozen clock, anchor the string to an absolute date.
            anchor = self.pick(["2000-01-01", "1999-12-31 23:59:59", "2020-02-29 12:00",
                                "@1234567890", "1970-01-01 00:00 UTC", "2021-03-28"])
            s = f"{anchor} {s}" if self.chance(0.5) else f"{s} {anchor}"
        b = s.encode("utf-8", "surrogateescape")
        if self.chance(0.01):
            # A raw byte that is not UTF-8.
            i = self.r.randrange(len(b) + 1)
            b = b[:i] + bytes([self.r.randrange(0x80, 0x100)]) + b[i:]
        return b


# Seeds -----------------------------------------------------------------------


def rust_literal(text):
    out, i = [], 0
    while i < len(text):
        c = text[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        n = text[i + 1]
        if n == "x":
            out.append(chr(int(text[i + 2:i + 4], 16)))
            i += 4
        elif n == "u":
            j = text.index("}", i)
            out.append(chr(int(text[i + 3:j], 16)))
            i = j + 1
        else:
            out.append({"n": "\n", "t": "\t", "r": "\r", "0": "\0"}.get(n, n))
            i += 2
    return "".join(out)


def repo_seeds():
    path = os.path.join(ROOT, "crates/dogfish/tests/gnu_compat_dates.rs")
    try:
        text = open(path, encoding="utf-8").read()
    except OSError:
        return []
    seeds = []
    for m in re.finditer(r'^\s*"((?:[^"\\]|\\.)*)",\s*$', text, re.M):
        s = rust_literal(m.group(1))
        if "\n" not in s and "\0" not in s:
            seeds.append(s)
    return seeds


def gnu_seeds(tree):
    """Strings from GNU's date and touch tests and gnulib's parse-datetime test."""
    seeds = []
    for path in glob.glob(os.path.join(tree, "tests/date/*")) + glob.glob(
            os.path.join(tree, "tests/touch/*.sh")):
        text = open(path, encoding="utf-8", errors="replace").read()
        var = dict(re.findall(r"^\s*my \$(\w+)\s*=\s*['\"]([^'\"]*)['\"];", text, re.M))
        for _ in range(3):
            var = {k: re.sub(r"\$\{?(\w+)\}?", lambda m: var.get(m.group(1), m.group(0)), v)
                   for k, v in var.items()}
        pat = (r"(?:(?<![\w-])-d\s*|--date[= ]\s*)(?:'([^']*)'|\"((?:[^\"\\]|\\.)*)\"|"
               r"([^\s'\"]+))|\bin\d*='([^']*)'")
        for m in re.finditer(pat, text):
            s = next(g for g in m.groups() if g is not None)
            s = re.sub(r"\$\{?(\w+)\}?", lambda mm: var.get(mm.group(1), mm.group(0)), s)
            s = s.replace('\\"', '"').replace("\\@", "@")
            if s and "$" not in s and "\n" not in s:
                seeds.append(s)
    c = os.path.join(tree, "gnulib-tests/test-parse-datetime.c")
    if os.path.exists(c):
        text = open(c, encoding="utf-8", errors="replace").read()
        for m in re.finditer(r'"((?:[^"\\\n]|\\.)*)"', text):
            s = m.group(1)
            if "%" in s or s.endswith(".h") or not s:
                continue
            s = s.encode("latin-1", "replace").decode("unicode_escape", "replace")
            seeds.append(s)
    return seeds


# GNU's strings, extracted once (--update-seeds) so that runs without a GNU
# source tree, such as CI's, start from them too.
SEEDS_FILE = os.path.join(ROOT, "util/date-fuzz-seeds.json")


def saved_seeds():
    try:
        with open(SEEDS_FILE, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return []


def find_gnu_tree(arg):
    for cand in [arg, os.path.join(ROOT, "target/gnu-one/coreutils-9.4"),
                 os.path.join(ROOT, "target/gnu/coreutils-9.4")]:
        if cand and os.path.isdir(os.path.join(cand, "tests/date")):
            return cand
    return None


def find_faketime():
    for pat in ["/usr/lib/*/faketime/libfaketime.so.1", "/usr/lib/faketime/libfaketime.so.1",
                "/usr/local/lib/faketime/libfaketime.so.1", "/usr/lib64/faketime/libfaketime.so.1"]:
        hits = sorted(glob.glob(pat))
        if hits:
            return hits[0]
    return None


# Running ----------------------------------------------------------------------

CFG = {}


def init_worker(cfg):
    CFG.update(cfg)
    # Each worker touches its files in a directory of its own.
    CFG["tmp"] = tempfile.mkdtemp(dir=cfg["tmp"])


def env_for(tz, now):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C"}
    if tz is not None:
        env["TZ"] = tz
    if CFG.get("faketime") and now is not None:
        env.update({"LD_PRELOAD": CFG["faketime"], "FAKETIME_FMT": "%s",
                    "FAKETIME": now, "FAKETIME_DONT_FAKE_MONOTONIC": "1"})
    return env


def run(argv0, exe, args, env, stdin=None, cwd=None):
    """(exit status, stdout, stderr) of EXE ARGS run as ARGV0."""
    try:
        p = subprocess.run([argv0] + args, executable=exe, input=stdin, env=env,
                           capture_output=True, timeout=20, cwd=cwd)
        return (p.returncode, p.stdout, p.stderr)
    except subprocess.TimeoutExpired:
        return ("timeout", b"", b"")


def run_date(which, args, env, stdin=None):
    if which == "gnu":
        return run("date", CFG["gnu_date"], args, env, stdin)
    # Run as argv[0] "date", so both sides name themselves alike.
    return run("date", CFG["dogfish"], args, env, stdin)


def date_case(tz, now, args, lines):
    """Divergences for one `date` case: [(string, ours, theirs)], or []."""
    env = env_for(tz, now)
    if len(lines) == 1 and "-f" not in args:
        argv = [a if a is not None else lines[0] for a in args]
        ours, gnu = run_date("ours", argv, env), run_date("gnu", argv, env)
        return [] if ours == gnu else [(lines[0], ours, gnu)]
    stdin = b"\n".join(lines) + b"\n"
    ours, gnu = run_date("ours", args, env, stdin), run_date("gnu", args, env, stdin)
    if ours == gnu:
        return []
    if len(lines) == 1:
        return [(lines[0], ours, gnu)]
    # Bisect to the strings that differ on their own.
    mid = len(lines) // 2
    found = date_case(tz, now, args, lines[:mid]) + date_case(tz, now, args, lines[mid:])
    if found:
        return found
    # Only together: one line leaves state behind that changes another.
    small = minimize_lines(env, args, lines)
    stdin = b"\n".join(small) + b"\n"
    small_ours, small_gnu = run_date("ours", args, env, stdin), run_date("gnu", args, env, stdin)
    if small_ours == small_gnu:
        # It did not reproduce: keep the original, which confirmed() rejects.
        return [(b"\n".join(lines), ours, gnu)]
    return [(b"\n".join(small), small_ours, small_gnu)]


def minimize_lines(env, args, lines):
    """The fewest of LINES, in order, on which `date -f -` still diverges."""

    def diverges(trial):
        stdin = b"\n".join(trial) + b"\n"
        return run_date("ours", args, env, stdin) != run_date("gnu", args, env, stdin)

    n = 2
    while len(lines) >= 2:
        chunk = max(1, len(lines) // n)
        for i in range(0, len(lines), chunk):
            trial = lines[:i] + lines[i + chunk:]
            if trial and diverges(trial):
                lines, n = trial, max(n - 1, 2)
                break
        else:
            if chunk == 1:
                break
            n = min(n * 2, len(lines))
    return lines


def touch_case(tz, now, s):
    env = env_for(tz, now)
    plain = env_for(tz, None)
    results = []
    for which in ("ours", "gnu"):
        f = os.path.join(CFG["tmp"], "f")
        if os.path.lexists(f):
            os.unlink(f)
        exe = CFG["gnu_touch"] if which == "gnu" else CFG["dogfish"]
        st, out, err = run("touch", exe, ["-d", s, "f"], env, cwd=CFG["tmp"])
        stamp = b""
        if os.path.exists(f):
            stamp = subprocess.run([CFG["gnu_stat"], "-c", "%x|%y", f], env=plain,
                                   capture_output=True).stdout
            st_ns = os.stat(f).st_mtime_ns
            # "-d now" (or any string meaning exactly now) sets the kernel's
            # current time, which differs between the two runs.
            if abs(st_ns - time.time_ns()) < 30 * 10**9:
                stamp = b"<now>"
        results.append((st, out, err, stamp))
    ours, gnu = results
    return [] if ours == gnu else [(s, ours, gnu)]


def case_args(rng):
    args = []
    if rng.random() < 0.5:
        args.append("--debug")
    if rng.random() < 0.1:
        args.append("-u")
    return args


def work(task):
    """Runs case TASK; returns (kind, strings, divergences)."""
    index, kind = task
    rng = random.Random(f"{CFG['seed']}:{index}")
    gen = Gen(rng, CFG["seeds"], bool(CFG.get("faketime")))
    tz = rng.choices(TZS, TZ_WEIGHTS)[0]
    if rng.random() < 0.03:
        tz = None  # TZ unset
    now = None
    if CFG.get("faketime"):
        now = rng.choice(NOWS) if rng.random() < 0.6 else str(rng.randrange(1, 4102444800))
        if rng.random() < 0.3:
            now += f".{rng.randrange(10**9):09d}" if "." not in now else ""
    if kind == "touch":
        s = gen.string()
        while b"\n" in s or b"\0" in s:
            s = gen.string()
        out = touch_case(tz, now, s)
        return kind, 1, [dict(kind=kind, tz=tz, now=now, args=[], string=s, ours=o, gnu=g)
                         for s, o, g in out]
    fmt = rng.choice(FORMATS)
    args = case_args(rng)
    if "-u" in args and now is not None:
        # libfaketime initializes glibc's zone from TZ before GNU date's
        # putenv("TZ=UTC0") for -u, which then never takes effect. Setting
        # TZ=UTC0 as well means the same and keeps GNU honest.
        tz = "UTC0"
    if kind == "file":
        lines = [gen.string() for _ in range(rng.randrange(20, 120))]
        lines = [line.replace(b"\n", b" ") for line in lines]
        if rng.random() < 0.02:
            lines[rng.randrange(len(lines))] += b"\0junk"
        args = args + ["-f", "-"] + fmt
    else:
        s = gen.string()
        while b"\0" in s:
            s = gen.string()
        lines = [s]
        args = args + ["-d", None] + fmt
    found = []
    try:
        found = date_case(tz, now, [a for a in args], lines)
    except Exception as e:  # report it rather than kill the pool
        found = [(b"", ("error", repr(e).encode(), b""), ("error", b"", b""))]
    return kind, len(lines), [
        dict(kind=kind if confirmed(kind, tz, now, args, s, o, g) else "flaky-" + kind,
             tz=tz, now=now, args=args, string=s, ours=o, gnu=g)
        for s, o, g in found]


def confirmed(kind, tz, now, args, s, ours, gnu):
    """Whether a divergence reproduces exactly, twice, a moment apart.

    A one-off, such as a process that libfaketime failed to set up (it exits
    when a stale shared-memory file has its PID), is reported as flaky rather
    than as a divergence.
    """
    env = env_for(tz, now)
    for _ in range(2):
        time.sleep(0.5)
        if kind == "touch":
            again = touch_case(tz, now, s)
            if not again or again[0][1:] != (ours, gnu):
                return False
            continue
        if kind == "file":
            stdin, argv = s + b"\n", args
        else:
            stdin, argv = None, [a if a is not None else s for a in args]
        if (run_date("ours", argv, env, stdin), run_date("gnu", argv, env, stdin)) != (ours, gnu):
            return False
    return True


def repro(d, cfg):
    env = []
    if d["tz"] is not None:
        env.append(f"TZ={shlex.quote(d['tz'])}")
    if d["now"] is not None and cfg.get("faketime"):
        env += [f"LD_PRELOAD={cfg['faketime']}", "FAKETIME_FMT=%s",
                f"FAKETIME={d['now']}"]
    s = d["string"].decode("utf-8", "surrogateescape")
    if d["kind"] == "touch":
        return " ".join(env + ["touch", "-d", shlex.quote(s), "f;", "stat", "-c", "'%x|%y'", "f"])
    if d["kind"] == "file":
        args = " ".join(shlex.quote(a) for a in d["args"])
        return f"printf '%s\\n' {shlex.quote(s)} | " + " ".join(env) + f" date {args}"
    args = " ".join(shlex.quote(a if a is not None else s) for a in d["args"])
    return " ".join(env) + f" date {args}"


def show(d, cfg):
    def fmt(r):
        return "\n".join(f"  {name}: {v!r}" for name, v in zip(
            ("status", "stdout", "stderr", "stamp"), r))
    return (f"DIVERGENCE ({d['kind']}): {d['string']!r}\n  repro: {repro(d, cfg)}\n"
            f" dogfish:\n{fmt(d['ours'])}\n GNU:\n{fmt(d['gnu'])}\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--minutes", type=float)
    ap.add_argument("--seconds", type=float)
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 2)
    ap.add_argument("--seed", default=str(int(time.time())))
    ap.add_argument("--dogfish", default=os.path.join(ROOT, "target/release/dogfish"))
    ap.add_argument("--gnu-date", default="/usr/bin/date")
    ap.add_argument("--gnu-touch", default="/usr/bin/touch")
    ap.add_argument("--gnu-stat", default="/usr/bin/stat")
    ap.add_argument("--gnu-tree", help="GNU coreutils 9.4 source tree for seeds")
    ap.add_argument("--update-seeds", action="store_true",
                    help=f"rewrite {os.path.relpath(SEEDS_FILE, ROOT)} from the GNU tree and exit")
    ap.add_argument("--no-faketime", action="store_true", help="do not pin the clock")
    ap.add_argument("--out", help="append divergences to this file as JSON lines")
    ap.add_argument("--stop-after", type=int, default=50,
                    help="stop after this many divergences (0: never)")
    a = ap.parse_args()
    budget = a.seconds if a.seconds is not None else (a.minutes if a.minutes is not None else 1) * 60

    for exe in (a.dogfish, a.gnu_date, a.gnu_touch, a.gnu_stat):
        if not os.access(exe, os.X_OK):
            sys.exit(f"date-fuzz: {exe} is missing (build with `cargo build --release`)")
    version = subprocess.run([a.gnu_date, "--version"], capture_output=True, text=True).stdout
    if "GNU coreutils" not in version:
        sys.exit(f"date-fuzz: {a.gnu_date} is not GNU coreutils date")
    if "9.4" not in version.splitlines()[0]:
        print(f"date-fuzz: warning: comparing against {version.splitlines()[0]}, not 9.4",
              file=sys.stderr)

    tree = find_gnu_tree(a.gnu_tree)
    if a.update_seeds:
        if not tree:
            sys.exit("date-fuzz: --update-seeds needs a GNU source tree (--gnu-tree)")
        with open(SEEDS_FILE, "w", encoding="utf-8") as f:
            json.dump(sorted(set(gnu_seeds(tree))), f, indent=0, ensure_ascii=False)
            f.write("\n")
        print(f"date-fuzz: wrote {SEEDS_FILE}")
        return
    seeds = repo_seeds() + saved_seeds() + (gnu_seeds(tree) if tree else [])
    seeds = sorted(set(seeds))
    faketime = None if a.no_faketime else find_faketime()
    print(f"date-fuzz: seed {a.seed}, {len(seeds)} seed strings"
          f"{' (with ' + tree + ')' if tree else ''}, clock "
          f"{'pinned with ' + faketime if faketime else 'not pinned: strings name a date'}, "
          f"{a.jobs} jobs, {budget:.0f}s", flush=True)

    tmp = tempfile.mkdtemp(prefix="date-fuzz-")
    cfg = dict(seed=a.seed, seeds=seeds, faketime=faketime, tmp=tmp,
               dogfish=os.path.abspath(a.dogfish), gnu_date=a.gnu_date,
               gnu_touch=a.gnu_touch, gnu_stat=a.gnu_stat)
    start = time.monotonic()
    deadline = start + budget

    def tasks():
        i = 0
        while time.monotonic() < deadline:
            r = i % 20
            yield (i, "touch" if r == 0 else "arg" if r < 6 else "file")
            i += 1

    counts = {"file": [0, 0], "arg": [0, 0], "touch": [0, 0]}
    divergences = flaky = 0
    last_report = start
    out = open(a.out, "a", encoding="utf-8") if a.out else None
    with multiprocessing.Pool(a.jobs, init_worker, (cfg,)) as pool:
        for kind, n, found in pool.imap_unordered(work, tasks(), chunksize=4):
            counts[kind][0] += 1
            counts[kind][1] += n
            for d in found:
                if d["kind"].startswith("flaky-"):
                    flaky += 1
                    print(f"date-fuzz: flaky, not reproduced: {d['string'][:200]!r}", flush=True)
                else:
                    divergences += 1
                    print(show(d, cfg), flush=True)
                if out:
                    out.write(json.dumps({k: (v.decode("utf-8", "surrogateescape")
                                              if isinstance(v, bytes) else
                                              [x.decode("utf-8", "surrogateescape")
                                               if isinstance(x, bytes) else x for x in v]
                                              if isinstance(v, tuple) else v)
                                          for k, v in d.items()}) + "\n")
                    out.flush()
            if a.stop_after and divergences >= a.stop_after:
                pool.terminate()
                break
            if time.monotonic() - last_report > 60:
                last_report = time.monotonic()
                print(f"date-fuzz: {time.monotonic() - start:.0f}s, "
                      f"{sum(c[1] for c in counts.values())} strings, "
                      f"{sum(c[0] for c in counts.values())} cases, "
                      f"{divergences} divergences", flush=True)
    shutil.rmtree(tmp, ignore_errors=True)
    elapsed = time.monotonic() - start
    cases = sum(c[0] for c in counts.values())
    strings = sum(c[1] for c in counts.values())
    print(f"date-fuzz: {elapsed:.0f}s: {strings} strings in {cases} cases "
          f"(date -f: {counts['file'][1]} strings in {counts['file'][0]} cases; "
          f"date -d: {counts['arg'][0]}; touch -d: {counts['touch'][0]}); "
          f"{divergences} divergences, {flaky} flaky")
    sys.exit(1 if divergences else 0)


if __name__ == "__main__":
    main()
