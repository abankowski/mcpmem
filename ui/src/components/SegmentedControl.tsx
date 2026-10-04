interface SegmentedControlProps<Value extends string> {
  label: string;
  name: string;
  value: Value;
  options: readonly { value: Value; label: string; disabled?: boolean }[];
  onChange: (value: Value) => void;
}

export function SegmentedControl<Value extends string>({ label, name, value, options, onChange }: SegmentedControlProps<Value>) {
  return (
    <fieldset className="ui-segments">
      <legend className="ui-visually-hidden">{label}</legend>
      {options.map((option) => (
        <label key={option.value} className="ui-segments__option">
          <input type="radio" name={name} value={option.value} checked={value === option.value}
            disabled={option.disabled} onChange={() => onChange(option.value)} />
          <span>{option.label}</span>
        </label>
      ))}
    </fieldset>
  );
}
