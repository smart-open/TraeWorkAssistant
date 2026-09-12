import type { GroupView } from '../../types';

export function GroupSelect({
  value,
  groups,
  onChange,
}: {
  value: string | null;
  groups: GroupView[];
  onChange: (gid: string | null) => void;
}) {
  return (
    <select
      value={value ?? ''}
      onChange={(e) => onChange(e.target.value || null)}
      className="input !py-1 !text-xs w-32"
    >
      <option value="">未分组</option>
      {groups.map((g) => (
        <option key={g.id} value={g.id}>
          {g.name}
        </option>
      ))}
    </select>
  );
}
