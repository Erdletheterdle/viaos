import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ago, gb, tool } from "../lib";
import { useIsAdmin } from "../main";
import { Button, Copy, Empty, Failure, Field, Headline, Input, Section } from "../ui";
import { useFleets } from "./fleet";

type Image = { file: string; download: string; size_mb: number; created_at?: number };

const ARCHES = [
  { id: "x86_64", name: "PC", detail: "x86_64 · industrial boxes, NUCs, anything UEFI" },
  { id: "aarch64", name: "Raspberry Pi", detail: "aarch64 · Pi 4, 400, CM4" },
];

function Written({ image }: { image: Image }) {
  const cmd = `curl -sL ${image.download} | zstdcat | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync`;
  return (
    <div className="border-b border-rule py-4">
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <span className="font-mono text-sm font-medium break-all">{image.file}</span>
        <span className="font-mono text-xs text-ink-3">
          {gb(image.size_mb)}
          {image.created_at ? ` · ${ago(image.created_at)}` : ""}
        </span>
      </div>
      <div className="mt-3 flex flex-wrap gap-2">
        <a href={image.download} className="inline-flex h-9 items-center bg-act px-3.5 text-sm font-medium text-act-ink hover:opacity-85">
          Download
        </a>
        <Copy text={cmd} label="Copy write command" />
      </div>
    </div>
  );
}

export function Enroll() {
  const admin = useIsAdmin();
  const qc = useQueryClient();
  const fleets = useFleets();
  const [fleet, setFleet] = useState("");
  const [hostname, setHostname] = useState("");
  const [arch, setArch] = useState("x86_64");
  const [keys, setKeys] = useState("");
  const images = useQuery({
    queryKey: ["images"],
    queryFn: () => tool<{ bases: string[]; images: Image[] }>("list_provisioning_images"),
    refetchInterval: 15000,
  });
  const make = useMutation({
    mutationFn: () =>
      tool<Image>("create_provisioning_image", {
        fleet: fleet.trim(),
        arch,
        ...(hostname.trim() ? { hostname: hostname.trim() } : {}),
        ssh_keys: keys.split("\n").map((k) => k.trim()).filter(Boolean),
      }),
    onSuccess: () => {
      setHostname("");
      qc.invalidateQueries({ queryKey: ["images"] });
      qc.invalidateQueries({ queryKey: ["fleets"] });
    },
  });
  const bases = images.data?.bases;
  const noBase = bases && !bases.includes(arch);

  return (
    <>
      <Headline>Make an image, boot it once, and the device <span className="marked">joins by itself</span>.</Headline>
      <div className="grid gap-x-12 gap-y-8 lg:grid-cols-[minmax(0,26rem)_minmax(0,1fr)]">
        <form
          className="space-y-5"
          onSubmit={(e) => {
            e.preventDefault();
            make.mutate();
          }}
        >
          <div>
            <span className="label mb-1.5 block">Hardware</span>
            <div className="grid grid-cols-2 border border-rule">
              {ARCHES.map((a, i) => (
                <button
                  key={a.id}
                  type="button"
                  aria-pressed={arch === a.id}
                  onClick={() => setArch(a.id)}
                  className={`cursor-pointer p-3 text-left ${i ? "border-l border-rule" : ""} ${arch === a.id ? "bg-act text-act-ink" : "hover:bg-sheet"}`}
                >
                  <span className="block font-display text-lg font-semibold tracking-tight">{a.name}</span>
                  <span className="mt-0.5 block text-xs opacity-75">{a.detail}</span>
                </button>
              ))}
            </div>
          </div>
          <Field label="Fleet" hint="Created if it doesn't exist.">
            <Input list="fleet-names" required value={fleet} onChange={(e) => setFleet(e.target.value)} placeholder="line-a" />
            <datalist id="fleet-names">
              {fleets.data?.map((f) => <option key={f.name} value={f.name} />)}
            </datalist>
          </Field>
          <Field label="Name" hint="Optional. The device keeps the name it reports otherwise.">
            <Input value={hostname} onChange={(e) => setHostname(e.target.value)} placeholder="line-07" />
          </Field>
          <Field label="SSH keys" hint="Optional, one per line. Allowed to log in as root.">
            <textarea value={keys} onChange={(e) => setKeys(e.target.value)} rows={2} className="w-full border border-rule bg-sheet p-2.5 font-mono text-xs text-ink focus:border-rule-strong focus:outline-none" placeholder="ssh-ed25519 AAAA…" />
          </Field>
          {noBase && (
            <p className="border-l-2 border-warn py-1 pl-3 text-sm">
              No {arch} base image on this server yet. Upload one with <code className="font-mono text-xs">via base-image</code>.
            </p>
          )}
          <Failure error={make.error} />
          <Button tone="primary" type="submit" disabled={!admin || !fleet.trim() || make.isPending || !!noBase}>
            {make.isPending ? "Making it, up to a minute…" : "Make image"}
          </Button>
          {!admin && <p className="text-xs text-ink-3">Viewers can't make images.</p>}
        </form>

        <div className="min-w-0">
          <Section title="Ready to write">
            <Failure error={images.error} />
            {images.data?.images.length === 0 && (
              <Empty title="No images waiting.">Each one carries a single-use token, so it's for one device, and it's deleted once that device enrolls.</Empty>
            )}
            {images.data?.images
              .slice()
              .sort((a, b) => (b.created_at ?? 0) - (a.created_at ?? 0))
              .map((i) => <Written key={i.download} image={i} />)}
          </Section>
        </div>
      </div>
    </>
  );
}
