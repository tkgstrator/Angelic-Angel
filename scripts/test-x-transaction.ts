// 上流 (vendor/x-transaction) と Rust 移植の比較ベクタを検証する。--write で再生成。
// リポジトリルートで `bun scripts/test-x-transaction.ts`。ネットワーク・実 Cookie は使わない。
import { readFileSync, writeFileSync } from "node:fs";
import {
	getOndemandFileUrl,
	Transaction,
} from "../vendor/x-transaction/src/transaction.ts";

const VECTORS_PATH = new URL("./x-transaction-vectors.json", import.meta.url);

// ---- 上流の内部挙動を観測するプローブ (結果は変えない) ------------------------------------

const probe = {
	negativeRound: 0,
	cosNegative: 0,
	sinNegative: 0,
	cosOne: 0,
	sinZero: 0,
};
const realRound = Math.round;
const realCos = Math.cos;
const realSin = Math.sin;
Math.round = (x: number) => {
	if (x < 0 && !Number.isInteger(x)) probe.negativeRound++;
	return realRound(x);
};
Math.cos = (x: number) => {
	const v = realCos(x);
	if (v < 0) probe.cosNegative++;
	if (v === 1) probe.cosOne++;
	return v;
};
Math.sin = (x: number) => {
	const v = realSin(x);
	if (v < 0) probe.sinNegative++;
	if (v === 0) probe.sinZero++;
	return v;
};

// ---- 決定的な入力の生成 ---------------------------------------------------------------------

function mulberry32(seed: number): () => number {
	let a = seed >>> 0;
	return () => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

type KeyStyle = "padded" | "unpadded" | "spaced";
type SvgStyle = "plain" | "shouty" | "junkd";

interface Spec {
	name: string;
	seed: number;
	frameIndex: number;
	row: number;
	rowIndex: number;
	// keyBytes[index] の下位 4bit (= % 16) に設定する値
	nibbles: Record<number, number>;
	keyStyle: KeyStyle;
	svgStyle: SvgStyle;
	// 先頭に 1 本だけパスを持つ svg (frame に数えられない) を挟む
	decoySvg?: boolean;
}

const KEY_LENGTH = 70;
const ROW_COUNT = 16;
const ROW_NUMBERS = 11;

function toBase64(bytes: number[], style: KeyStyle): string {
	const padded = btoa(String.fromCharCode(...bytes));
	if (style === "unpadded") return padded.replace(/=+$/, "");
	if (style === "spaced")
		return `${padded.slice(0, 7)} \n ${padded.slice(7, 40)}\t${padded.slice(40)}`;
	return padded;
}

function pathData(rows: number[][]): string {
	// 先頭 9 文字 ("M 10,10 C") は上流が読み飛ばす
	return `M 10,10 C ${rows.map((row) => row.join(",")).join(" C ")}`;
}

function svg(index: number, data: string, style: SvgStyle): string {
	switch (style) {
		case "plain":
			return `<svg id="loading-x-anim-${index}" viewBox="0 0 32 32"><g><path d="M0 0L1 1"/><path d="${data}"/></g></svg>`;
		case "shouty":
			return `<SVG class="a"\n  ID='LOADING-X-ANIM-${index + 10}'\n viewBox="0 0 1 1"><PATH fill='none' D='M0 0'/><PATH D='${data}' FILL='none'/></SVG>`;
		case "junkd":
			// 属性名 id の末尾 d= にも一致するため、上流の貪欲な取り方では id の値が選ばれる
			return `<svg viewBox="0 0 8 8" id="loading-x-anim-${index}"><path d="junk"/><path d="junk" id="${data}"/></svg>`;
	}
}

interface Built {
	html: string;
	ondemand: string;
	keyBytes: number[];
}

function build(spec: Spec): Built {
	const rand = mulberry32(spec.seed);
	const byte = () => Math.floor(rand() * 256);
	const keyBytes = Array.from({ length: KEY_LENGTH }, byte);
	keyBytes[5] = (keyBytes[5] & 0xfc) | spec.frameIndex;
	keyBytes[spec.rowIndex] = (keyBytes[spec.rowIndex] & 0xf0) | spec.row;
	for (const [index, nibble] of Object.entries(spec.nibbles)) {
		keyBytes[Number(index)] = (keyBytes[Number(index)] & 0xf0) | nibble;
	}

	// 選ばれる frame だけ 16 行 x 11 個、それ以外は 1 行
	const frames = [0, 1, 2, 3].map((index) => {
		const rowCount = index === spec.frameIndex ? ROW_COUNT : 1;
		const rows = Array.from({ length: rowCount }, () =>
			Array.from({ length: ROW_NUMBERS }, byte),
		);
		return pathData(rows);
	});
	const svgs = frames.map((data, index) => svg(index, data, spec.svgStyle));
	if (spec.decoySvg) {
		svgs.unshift('<svg id="loading-x-anim-9"><path d="M 1 1 C 1,2,3"/></svg>');
	}
	const key = toBase64(keyBytes, spec.keyStyle);
	const html = [
		"<!DOCTYPE html><html><head>",
		spec.keyStyle === "spaced"
			? `<META NAME='twitter-site-verification'\n CONTENT='${key}'>`
			: `<meta name="twitter-site-verification" content="${key}">`,
		'<svg id="not-ours"><path d="a"/><path d="b"/></svg>',
		"</head><body>",
		'<script>{20112:"react-syntax",20113:"ondemand.s",20226:"other"}</script>',
		'<script>{20112:"abc111",20113:"c0ffee1",20226:"fff999"}</script>',
		...svgs,
		"</body></html>",
	].join("\n");

	const indices = Object.keys(spec.nibbles);
	const ondemand = [
		'"use strict";var x=function(e){',
		"(ee[3], 16);(e[123],16);",
		`var r=(e[${spec.rowIndex}], 16);`,
		...indices.map((index) => `var k${index}=(e[${index}],  16);`),
		"}",
	].join("");
	return { html, ondemand, keyBytes };
}

// ---- ケース一覧 -----------------------------------------------------------------------------

const specs: Spec[] = [
	{
		name: "frame0-row3",
		seed: 1,
		frameIndex: 0,
		row: 3,
		rowIndex: 11,
		nibbles: { 10: 7, 20: 9, 30: 11 },
		keyStyle: "padded",
		svgStyle: "plain",
	},
	{
		name: "frame1-row15",
		seed: 2,
		frameIndex: 1,
		row: 15,
		rowIndex: 7,
		nibbles: { 12: 13, 22: 14, 32: 15 },
		keyStyle: "unpadded",
		svgStyle: "plain",
	},
	{
		name: "frame2-row0",
		seed: 3,
		frameIndex: 2,
		row: 0,
		rowIndex: 40,
		nibbles: { 14: 4, 24: 6 },
		keyStyle: "padded",
		svgStyle: "shouty",
	},
	{
		name: "frame3-row9",
		seed: 4,
		frameIndex: 3,
		row: 9,
		rowIndex: 62,
		nibbles: { 15: 3, 25: 3, 35: 3, 45: 3 },
		keyStyle: "spaced",
		svgStyle: "plain",
	},
	{
		name: "time-zero-no-key-indices",
		seed: 5,
		frameIndex: 1,
		row: 5,
		rowIndex: 33,
		nibbles: {},
		keyStyle: "padded",
		svgStyle: "plain",
	},
	{
		name: "time-zero-nibble-zero",
		seed: 6,
		frameIndex: 2,
		row: 8,
		rowIndex: 9,
		nibbles: { 17: 0, 27: 9 },
		keyStyle: "padded",
		svgStyle: "plain",
	},
	{
		name: "time-past-one",
		seed: 7,
		frameIndex: 0,
		row: 12,
		rowIndex: 21,
		nibbles: { 13: 15, 23: 15, 33: 15, 43: 15, 53: 15 },
		keyStyle: "padded",
		svgStyle: "plain",
	},
	{
		name: "custom-round-tie-5",
		seed: 8,
		frameIndex: 3,
		row: 2,
		rowIndex: 50,
		nibbles: { 16: 5 },
		keyStyle: "unpadded",
		svgStyle: "shouty",
	},
	{
		name: "custom-round-tie-15",
		seed: 9,
		frameIndex: 0,
		row: 6,
		rowIndex: 44,
		nibbles: { 18: 3, 28: 5 },
		keyStyle: "padded",
		svgStyle: "plain",
	},
	{
		name: "custom-round-tie-25",
		seed: 10,
		frameIndex: 1,
		row: 14,
		rowIndex: 3,
		nibbles: { 19: 5, 29: 5 },
		keyStyle: "padded",
		svgStyle: "plain",
	},
	{
		name: "junk-d-attribute-and-decoy-svg",
		seed: 11,
		frameIndex: 2,
		row: 10,
		rowIndex: 25,
		nibbles: { 31: 8, 41: 6, 51: 7 },
		keyStyle: "padded",
		svgStyle: "junkd",
		decoySvg: true,
	},
	{
		name: "shouty-spaced-key",
		seed: 12,
		frameIndex: 3,
		row: 7,
		rowIndex: 66,
		nibbles: { 36: 12, 46: 11 },
		keyStyle: "spaced",
		svgStyle: "shouty",
		decoySvg: true,
	},
	...Array.from(
		{ length: 12 },
		(_, i): Spec => ({
			name: `random-${i}`,
			seed: 100 + i,
			frameIndex: i % 4,
			row: (i * 5 + 1) % 16,
			rowIndex: 10 + i * 4,
			nibbles: {
				[20 + i]: (i * 7 + 3) % 16,
				[40 + i]: (i * 3 + 5) % 16,
				[60 + i]: (i * 11 + 1) % 16,
			},
			keyStyle: "padded",
			svgStyle: "plain",
		}),
	),
];

// 上流が例外を投げる入力。Rust も構築に失敗しなければならない。
function failures(): { name: string; html: string; ondemand: string }[] {
	const base = build(specs[0]);
	const noMeta = base.html.replace(/<meta[^>]*>/i, "");
	return [
		{
			name: "throws-no-verification-key",
			html: noMeta,
			ondemand: base.ondemand,
		},
		{
			name: "throws-invalid-base64-key",
			html: base.html.replace(/content="[^"]*"/, 'content="!!!!"'),
			ondemand: base.ondemand,
		},
		{
			name: "throws-base64-length-1-mod-4",
			html: base.html.replace(/content="[^"]*"/, 'content="AAAAA"'),
			ondemand: base.ondemand,
		},
		{
			name: "throws-no-key-byte-indices",
			html: base.html,
			ondemand: "var x=1;",
		},
		{
			name: "throws-no-animation-frames",
			html: base.html.replace(/<svg id="loading[\s\S]*?<\/svg>/gi, ""),
			ondemand: base.ondemand,
		},
		{
			// frameIndex 3 を要求するのに frame が 2 つしかない
			name: "throws-frame-index-out-of-range",
			html: (() => {
				const built = build(specs[3]);
				return built.html.replace(
					/<svg id="loading-x-anim-[23][\s\S]*?<\/svg>/g,
					"",
				);
			})(),
			ondemand: build(specs[3]).ondemand,
		},
		{
			name: "throws-row-index-beyond-key",
			html: base.html,
			ondemand: base.ondemand.replace("(e[11], 16)", "(e[99], 16)"),
		},
	];
}

const REQUESTS = [
	{ method: "GET", path: "/1.1/account/settings.json" },
	{ method: "POST", path: "/i/api/1.1/friendships/update.json" },
	{ method: "GET", path: "/i/api/1.1/friends/ids.json" },
	{ method: "GET", path: "/1.1/account/settings.json" },
	{ method: "POST", path: "/i/api/1.1/friends/ids.json" },
];
const TIMES = [123_456_789, -1234, 3_000_000_000, 0, 2_147_483_648];
const XORS = [0x5a, 0, 255, 1, 128];

// ---- ベクタ生成 -----------------------------------------------------------------------------

interface Id {
	method: string;
	path: string;
	time: number;
	xor: number;
	id: string;
}

interface Case {
	name: string;
	kind: "transaction" | "throws" | "ondemand-url";
	html: string;
	ondemand: string;
	ondemandUrl: string | null;
	constructs: boolean;
	animationKey: string | null;
	ids: Id[];
}

const coverage = {
	frameIndices: new Set<number>(),
	rows: new Set<number>(),
	timeZero: 0,
	timeInRange: 0,
	timePastOne: 0,
	emptyKeyIndices: 0,
};

async function generate(): Promise<Case[]> {
	const cases: Case[] = [];
	const inputs = [
		...specs.map((spec) => ({ spec, ...build(spec) })),
		...failures().map((f) => ({ spec: undefined, ...f })),
	];

	for (const input of inputs) {
		const name = input.spec?.name ?? (input as { name: string }).name;
		const ondemandUrl = getOndemandFileUrl(input.html);
		let transaction: Transaction | undefined;
		try {
			transaction = new Transaction(input.html, input.ondemand);
		} catch {
			transaction = undefined;
		}
		if (input.spec && !transaction)
			throw new Error(`${name}: 上流が構築に失敗した`);
		if (!input.spec && transaction)
			throw new Error(`${name}: 失敗するはずの入力を上流が受理した`);

		const ids: Id[] = [];
		let animationKey: string | null = null;
		if (transaction) {
			// private フィールドは TS 上の制約でしかないため、観測のためだけに読む
			const internals = transaction as unknown as {
				animationKey: string;
				keyBytes: number[];
				keyBytesIndices: number[];
				calculateFrameTime(): number;
			};
			animationKey = internals.animationKey;
			coverage.frameIndices.add(internals.keyBytes[5] % 4);
			coverage.rows.add(
				internals.keyBytes[
					(transaction as unknown as { rowIndex: number }).rowIndex
				] % 16,
			);
			const targetTime = internals.calculateFrameTime() / 4096;
			if (targetTime <= 0) coverage.timeZero++;
			else if (targetTime < 1) coverage.timeInRange++;
			else coverage.timePastOne++;
			if (internals.keyBytesIndices.length === 0) coverage.emptyKeyIndices++;

			for (let i = 0; i < REQUESTS.length; i++) {
				const time = TIMES[i];
				const xor = XORS[i];
				Math.random = () => xor / 256;
				const id = await transaction.generateTransactionId(
					REQUESTS[i].method,
					REQUESTS[i].path,
					time,
				);
				ids.push({ ...REQUESTS[i], time, xor, id });
			}
		}
		cases.push({
			name,
			kind: transaction ? "transaction" : "throws",
			html: input.html,
			ondemand: input.ondemand,
			ondemandUrl,
			constructs: transaction !== undefined,
			animationKey,
			ids,
		});
	}
	return cases;
}

function assertCoverage(cases: Case[]): void {
	const problems: string[] = [];
	if (coverage.frameIndices.size !== 4)
		problems.push(
			`frame index が 4 通り揃っていない: ${[...coverage.frameIndices]}`,
		);
	if (coverage.rows.size < 8)
		problems.push(`row が 8 通り未満: ${[...coverage.rows]}`);
	if (coverage.timeZero === 0)
		problems.push("Cubic の time <= 0 分岐を通っていない");
	if (coverage.timeInRange === 0)
		problems.push("Cubic の 0 < time < 1 分岐を通っていない");
	if (coverage.timePastOne === 0)
		problems.push("Cubic の time >= 1 分岐を通っていない");
	if (coverage.emptyKeyIndices === 0)
		problems.push("key byte index が空のケースが無い");
	if (probe.negativeRound === 0)
		problems.push("Math.round に負の小数が渡っていない");
	if (probe.cosNegative === 0 || probe.sinNegative === 0)
		problems.push("負の cos / sin が出ていない");
	if (probe.cosOne === 0 || probe.sinZero === 0)
		problems.push("行列値がちょうど 1 / 0 になるケースが無い");
	if (!cases.some((c) => c.ids.some((id) => id.time < 0)))
		problems.push("負の timestamp が無い");
	if (!cases.some((c) => c.ids.some((id) => id.time > 2 ** 31)))
		problems.push("32bit を超える timestamp が無い");
	if (!cases.some((c) => c.ids.some((id) => id.method === "POST")))
		problems.push("POST が無い");
	if (!cases.some((c) => !c.constructs))
		problems.push("上流が失敗するケースが無い");
	if (!cases.some((c) => c.ondemandUrl === null))
		problems.push("ondemand URL が取れないケースが無い");
	if (problems.length > 0)
		throw new Error(`ベクタの網羅性が足りない:\n- ${problems.join("\n- ")}`);
}

// 上流テストと同じ ondemand URL の抽出ケースも固定する
function ondemandUrlCases(): Case[] {
	const htmls = [
		'<script>{20112:"react-syntax-highlighter_languages_refractor_sass",20113:"ondemand.s",20226:"other"}{20112:"abc111",20113:"abc123def",20226:"xyz999"}</script>',
		`<script>{999:'foo',20113:'ondemand.s'}{999:'bar',20113:'def456'}</script>`,
		'<script>{,20113:"ondemand.s"}</script>',
		"<html><body>No scripts here</body></html>",
		"",
		// 大文字の 16 進は取らない / 別のインデックスに部分一致しない
		'<script>,1:"ondemand.s",1:"ABCDEF"</script>',
		'<script>,12:"ondemand.s" ,112:"aaaa" ,12:"bbbb"</script>',
	];
	return htmls.map((html, i) => ({
		name: `ondemand-url-${i}`,
		kind: "ondemand-url" as const,
		html,
		ondemand: "",
		ondemandUrl: getOndemandFileUrl(html),
		constructs: false,
		animationKey: null,
		ids: [],
	}));
}

const cases = [...(await generate()), ...ondemandUrlCases()];
assertCoverage(cases);
const counts = {
	transaction: cases.filter((c) => c.kind === "transaction").length,
	throws: cases.filter((c) => c.kind === "throws").length,
	ondemandUrl: cases.filter((c) => c.kind === "ondemand-url").length,
	ids: cases.reduce((total, c) => total + c.ids.length, 0),
};
const json = `${JSON.stringify({ counts, cases }, null, 1)}\n`;

if (process.argv.includes("--write")) {
	writeFileSync(VECTORS_PATH, json);
	console.log(
		`wrote ${cases.length} cases to scripts/x-transaction-vectors.json`,
	);
} else {
	const current = readFileSync(VECTORS_PATH, "utf8");
	if (current !== json) {
		console.error(
			"scripts/x-transaction-vectors.json が上流の出力と一致しない。--write で再生成して差分を確認すること。",
		);
		process.exit(1);
	}
	console.log(`ok: ${cases.length} cases match upstream`);
}
