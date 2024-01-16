from typing import Any, Dict, Type, TypeVar, Tuple, Optional, BinaryIO, TextIO, TYPE_CHECKING

from typing import List


from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from typing import Union
from ..types import UNSET, Unset






T = TypeVar("T", bound="ListResourceTypeResponse200Item")


@_attrs_define
class ListResourceTypeResponse200Item:
    """ 
        Attributes:
            name (str):
            workspace_id (Union[Unset, str]):
            schema (Union[Unset, Any]):
            description (Union[Unset, str]):
     """

    name: str
    workspace_id: Union[Unset, str] = UNSET
    schema: Union[Unset, Any] = UNSET
    description: Union[Unset, str] = UNSET
    additional_properties: Dict[str, Any] = _attrs_field(init=False, factory=dict)


    def to_dict(self) -> Dict[str, Any]:
        name = self.name
        workspace_id = self.workspace_id
        schema = self.schema
        description = self.description

        field_dict: Dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
            "name": name,
        })
        if workspace_id is not UNSET:
            field_dict["workspace_id"] = workspace_id
        if schema is not UNSET:
            field_dict["schema"] = schema
        if description is not UNSET:
            field_dict["description"] = description

        return field_dict



    @classmethod
    def from_dict(cls: Type[T], src_dict: Dict[str, Any]) -> T:
        d = src_dict.copy()
        name = d.pop("name")

        workspace_id = d.pop("workspace_id", UNSET)

        schema = d.pop("schema", UNSET)

        description = d.pop("description", UNSET)

        list_resource_type_response_200_item = cls(
            name=name,
            workspace_id=workspace_id,
            schema=schema,
            description=description,
        )

        list_resource_type_response_200_item.additional_properties = d
        return list_resource_type_response_200_item

    @property
    def additional_keys(self) -> List[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties